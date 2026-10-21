//! Fresh `SQLite` representation of the semantic checkpoint store.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use futures_channel::oneshot;
use plakous_agent::internal::{
    AcquireBranch, ArchiveReceipt, ArchiveSession, BranchHead, BranchLease, BranchRevision,
    BranchTransitionMode, CheckpointBundleError, CheckpointId, CheckpointObjectBundle,
    CheckpointObjectRef, CommitCheckpoint, CommitOperation, CommitReceipt, CommittedCheckpoint,
    CreateOperation, CreateReceipt, CreateSession, DeleteReceipt, DeleteSession, Digest,
    DigestDomain, DurabilityReport, FeedBranchTransition, FeedChange, FeedLocator, FeedPage,
    FeedSeq, FeedTail, ForkBranch, ForkLineage, ForkOperation, ForkReceipt, LeaseClock,
    LeaseDomain, LeaseInstant, LoadedBranch, Page, PruneBranch, PruneReceipt, RecoverFork, RunId,
    RuntimeCheckpoint, RuntimeCheckpointKind, RuntimeDurability, RuntimeManifest, RuntimeObject,
    RuntimeObjectAddress, RuntimeObjectKind, SessionHead, SessionId, SessionQuery, SessionRef,
    SessionRevision, SessionStore, SessionStoreError, SessionSummary, SessionWorkflowId,
    StoreAdmission, StoreCapacityBound, StoreFuture, StoreInvalidReason, StoreUsage,
    StoredCheckpoint, Timestamp,
};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use super::lease_clock::SystemLeaseClock;

#[cfg(test)]
mod actor_tests;
#[cfg(test)]
mod admission_budget_tests;
mod checkpoint_cache;
mod feed;
mod gc;
#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod index_tests;
use super::{MemoryStoreLimits, expired, expiry, require_lease_domain, sample_clock};
use checkpoint_cache::CheckpointCache;
use plakous_agent::internal::{StoreReclaimMode, StoreReclaimReport};

const APPLICATION_ID: i64 = 0x504c_4b35;
const INCREMENTAL_AUTO_VACUUM: i64 = 2;
/// The runtime store's schema version. Only this exact version opens.
pub const SQLITE_SCHEMA_VERSION: i64 = 25;
const SCHEMA_VERSION: i64 = SQLITE_SCHEMA_VERSION;
/// Reviewed `SQLite` release statically embedded by the workspace dependency.
pub const BUNDLED_SQLITE_VERSION: &str = "3.53.2";
/// Upstream source identity of [`BUNDLED_SQLITE_VERSION`].
pub const BUNDLED_SQLITE_SOURCE_ID: &str =
    "2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24";
/// Security and concurrency options required from the reviewed bundled build.
pub const BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS: [&str; 4] = [
    "DEFAULT_FOREIGN_KEYS",
    "ENABLE_API_ARMOR",
    "THREADSAFE=1",
    "USE_URI",
];
const DEFAULT_QUEUE_CAPACITY: usize = 64;
const READ_QUEUE_TIMEOUT: Duration = Duration::from_secs(1);
const SLOW_JOB_THRESHOLD: Duration = Duration::from_millis(250);
const BUSY_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const BACKGROUND_VACUUM_PAGES: u32 = 1024;
const BACKGROUND_JOB_INTERVAL: usize = 32;
const CHECKPOINT_CACHE_BYTES: usize = 64 * 1024 * 1024;
type Job = Box<dyn FnOnce(&mut Backend) + Send + 'static>;

#[derive(Clone, Copy)]
enum Lane {
    Critical,
    Read,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutedJob {
    Critical,
    Read,
    Background,
}

impl Lane {
    const fn label(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Read => "read",
        }
    }
}

/// The least time between two overflow warnings of one lane.
const OVERFLOW_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Rate limit for one lane's queue-full warnings: the first overflow logs,
/// then at most one line per [`OVERFLOW_LOG_INTERVAL`] carrying the count.
#[derive(Default)]
struct OverflowLog {
    logged_at: Option<Instant>,
    unlogged: u64,
}

impl OverflowLog {
    /// Counts one overflow at `now`; returns the overflows the caller logs
    /// now, including this one, or `None` while the interval holds them.
    fn record(&mut self, now: Instant) -> Option<u64> {
        self.unlogged = self.unlogged.saturating_add(1);
        if self
            .logged_at
            .is_some_and(|at| now.saturating_duration_since(at) < OVERFLOW_LOG_INTERVAL)
        {
            return None;
        }
        self.logged_at = Some(now);
        Some(std::mem::take(&mut self.unlogged))
    }
}

struct ActorActivity {
    stopped: AtomicBool,
    depths: [AtomicUsize; 2],
    running: Mutex<Option<(&'static str, Instant)>>,
    read_space: tokio::sync::Semaphore,
}

impl Default for ActorActivity {
    fn default() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            depths: Default::default(),
            running: Mutex::new(None),
            read_space: tokio::sync::Semaphore::new(0),
        }
    }
}

impl ActorActivity {
    fn start(&self, label: &'static str) -> Instant {
        let started = Instant::now();
        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((label, started));
        started
    }

    fn finish(&self, lane: &'static str, label: &'static str, started: Instant) {
        let elapsed = started.elapsed();
        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if elapsed >= SLOW_JOB_THRESHOLD {
            tracing::warn!(
                target: "plakous_store::sqlite_actor",
                lane,
                job = label,
                elapsed_ms = elapsed.as_millis(),
                critical_depth = self.depths[0].load(Ordering::Relaxed),
                read_depth = self.depths[1].load(Ordering::Relaxed),
                "SQLite store actor job is slow"
            );
        }
    }
}

#[derive(Clone)]
struct ActorSender {
    critical: mpsc::SyncSender<Job>,
    read: mpsc::SyncSender<Job>,
    wake: mpsc::SyncSender<()>,
    capacity: usize,
    overflow: Arc<[Mutex<OverflowLog>; 2]>,
    activity: Arc<ActorActivity>,
}

struct ActorReceiver {
    critical: mpsc::Receiver<Job>,
    read: mpsc::Receiver<Job>,
    wake: mpsc::Receiver<()>,
    activity: Arc<ActorActivity>,
}

fn actor_queue(capacity: usize) -> (ActorSender, ActorReceiver) {
    let activity = Arc::new(ActorActivity {
        read_space: tokio::sync::Semaphore::new(capacity),
        ..ActorActivity::default()
    });
    let (critical_tx, critical_rx) = mpsc::sync_channel(capacity);
    let (read_tx, read_rx) = mpsc::sync_channel(capacity);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    (
        ActorSender {
            critical: critical_tx,
            read: read_tx,
            wake: wake_tx,
            capacity,
            overflow: Arc::default(),
            activity: Arc::clone(&activity),
        },
        ActorReceiver {
            critical: critical_rx,
            read: read_rx,
            wake: wake_rx,
            activity,
        },
    )
}

impl ActorSender {
    fn enqueue(&self, lane: Lane, job: Job) -> Result<(), mpsc::TrySendError<Job>> {
        if self.activity.stopped.load(Ordering::Acquire) {
            return Err(mpsc::TrySendError::Disconnected(job));
        }
        let sender = match lane {
            Lane::Critical => &self.critical,
            Lane::Read => &self.read,
        };
        // Count before publication: the actor may consume the job immediately.
        self.activity.depths[lane as usize].fetch_add(1, Ordering::Relaxed);
        if let Err(error) = sender.try_send(job) {
            self.activity.depths[lane as usize].fetch_sub(1, Ordering::Relaxed);
            return Err(error);
        }
        let _ = self.wake.try_send(());
        Ok(())
    }

    fn send(&self, lane: Lane, job: Job) -> Result<(), SessionStoreError> {
        let permit = if matches!(lane, Lane::Read) {
            Some(
                self.activity
                    .read_space
                    .try_acquire()
                    .map_err(|error| match error {
                        tokio::sync::TryAcquireError::Closed => {
                            SessionStoreError::BackendUnavailable
                        }
                        tokio::sync::TryAcquireError::NoPermits => {
                            self.overflowed(lane);
                            SessionStoreError::BackendBusy
                        }
                    })?,
            )
        } else {
            None
        };
        self.enqueue(lane, job)
            .map(|()| {
                if let Some(permit) = permit {
                    permit.forget();
                }
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    self.overflowed(lane);
                    SessionStoreError::BackendBusy
                }
                mpsc::TrySendError::Disconnected(_) => {
                    actor_lost(lane, "disconnected");
                    SessionStoreError::BackendUnavailable
                }
            })
    }

    fn send_read(&self, job: Job) -> StoreFuture<'_, ()> {
        let first = match self.activity.read_space.try_acquire() {
            Ok(permit) => {
                let result = self.enqueue(Lane::Read, job).map_err(|_| {
                    actor_lost(Lane::Read, "disconnected");
                    SessionStoreError::BackendUnavailable
                });
                if result.is_ok() {
                    permit.forget();
                }
                return Box::pin(async move { result });
            }
            Err(error) => error,
        };
        Box::pin(async move {
            let permit = match first {
                tokio::sync::TryAcquireError::Closed => {
                    actor_lost(Lane::Read, "disconnected");
                    return Err(SessionStoreError::BackendUnavailable);
                }
                tokio::sync::TryAcquireError::NoPermits => {
                    match tokio::time::timeout(
                        READ_QUEUE_TIMEOUT,
                        self.activity.read_space.acquire(),
                    )
                    .await
                    {
                        Ok(Ok(permit)) => permit,
                        Ok(Err(_)) => {
                            actor_lost(Lane::Read, "disconnected");
                            return Err(SessionStoreError::BackendUnavailable);
                        }
                        Err(_) => {
                            self.overflowed(Lane::Read);
                            return Err(SessionStoreError::BackendBusy);
                        }
                    }
                }
            };
            self.enqueue(Lane::Read, job).map_err(|_| {
                actor_lost(Lane::Read, "disconnected");
                SessionStoreError::BackendUnavailable
            })?;
            // Queue capacity becomes available at dequeue, not at job completion.
            permit.forget();
            Ok(())
        })
    }
}

impl ActorSender {
    /// Queue pressure is visible without logging session or request content.
    fn overflowed(&self, lane: Lane) {
        let log = &self.overflow[lane as usize];
        let overflows = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(Instant::now());
        if let Some(overflows) = overflows {
            let running = *self
                .activity
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (running_job, running_ms) =
                running.map_or(("idle", 0), |(label, at)| (label, at.elapsed().as_millis()));
            tracing::warn!(
                target: "plakous_store::sqlite_actor",
                lane = lane.label(),
                capacity = self.capacity,
                overflows,
                critical_depth = self.activity.depths[0].load(Ordering::Relaxed),
                read_depth = self.activity.depths[1].load(Ordering::Relaxed),
                running_job,
                running_ms,
                "SQLite store request lane is full"

            );
        }
    }
}

/// The actor thread exited (a job panicked): `disconnected` refuses a new
/// request, `reply_dropped` loses the reply of one already queued.
fn actor_lost(lane: Lane, reason: &'static str) {
    tracing::warn!(
        target: "plakous_store::sqlite_actor",
        lane = lane.label(),
        reason,
        "SQLite store actor is gone; request refused as unavailable"
    );
}

impl Drop for ActorReceiver {
    fn drop(&mut self) {
        self.activity.stopped.store(true, Ordering::Release);
        self.activity.read_space.close();
    }
}

impl ActorReceiver {
    fn background_step(&self, backend: &mut Backend) -> bool {
        let started = self.activity.start("background_step");
        let pending = backend.background_step();
        self.activity
            .finish("background", "background_step", started);
        pending
    }

    fn run(&self, backend: &mut Backend) {
        let mut foreground_jobs = 0;
        loop {
            if foreground_jobs >= BACKGROUND_JOB_INTERVAL {
                self.background_step(backend);
                foreground_jobs = 0;
            }
            let job = self
                .critical
                .try_recv()
                .map(|job| (Lane::Critical, job))
                .or_else(|_| self.read.try_recv().map(|job| (Lane::Read, job)));
            if let Ok((lane, job)) = job {
                self.activity.depths[lane as usize].fetch_sub(1, Ordering::Relaxed);
                if matches!(lane, Lane::Read) {
                    self.activity.read_space.add_permits(1);
                }
                job(backend);
                foreground_jobs += 1;
                continue;
            }
            if self.background_step(backend) {
                foreground_jobs = 0;
                continue;
            }
            if self.wake.recv().is_err() {
                break;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupStage {
    Header,
    PrivateFile,
    Actor,
    Connection,
    Runtime,
    Configuration,
    Schema,
    Admission,
    Ready,
}

impl StartupStage {
    const fn label(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::PrivateFile => "private_file",
            Self::Actor => "actor",
            Self::Connection => "connection",
            Self::Runtime => "runtime",
            Self::Configuration => "configuration",
            Self::Schema => "schema",
            Self::Admission => "admission",
            Self::Ready => "ready",
        }
    }
}

struct StartupProgress(Mutex<StartupStage>);

impl StartupProgress {
    fn set(&self, stage: StartupStage) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = stage;
    }

    fn failure(&self, category: &'static str, error: SessionStoreError) -> StartupFailure {
        StartupFailure {
            stage: *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            category,
            error,
        }
    }
}

#[derive(Debug)]
struct StartupFailure {
    stage: StartupStage,
    category: &'static str,
    error: SessionStoreError,
}

impl StartupFailure {
    fn report(self) -> SessionStoreError {
        // Static categories only: never format a database path, SQL, or backend source.
        tracing::warn!(
            target: "plakous_store::sqlite_startup",
            stage = self.stage.label(),
            category = self.category,
            store_error = startup_error_category(self.error),
            "SQLite store startup failed"
        );
        self.error
    }
}

const fn startup_error_category(error: SessionStoreError) -> &'static str {
    match error {
        SessionStoreError::BackendBusy => "backend_busy",
        SessionStoreError::BackendUnavailable => "backend_unavailable",
        SessionStoreError::UnsupportedSchema => "unsupported_schema",
        SessionStoreError::UnsupportedCodec => "unsupported_codec",
        SessionStoreError::Corrupt(_) => "corrupt",
        SessionStoreError::Invalid(_) => "invalid",
        SessionStoreError::Capacity(_) => "capacity",
        _ => "semantic",
    }
}

/// Bounded `SQLite` actor and retained-state options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteStoreOptions {
    limits: MemoryStoreLimits,
    queue_capacity: NonZeroUsize,
    complete_verify_on_open: bool,
}

impl Default for SqliteStoreOptions {
    fn default() -> Self {
        let memory = MemoryStoreLimits::default();
        Self {
            // Durable stores accumulate history across sessions, unlike an in-memory run.
            limits: MemoryStoreLimits::new(
                memory.max_sessions(),
                memory.max_branches_per_session(),
                nonzero(524_288),
                nonzero(4_194_304),
                nonzero(4 * 1024 * 1024 * 1024),
                memory.max_page_size(),
            ),
            queue_capacity: nonzero(DEFAULT_QUEUE_CAPACITY),
            complete_verify_on_open: false,
        }
    }
}

impl SqliteStoreOptions {
    /// Creates explicit semantic/object and actor-queue bounds.
    ///
    /// Open admits shape, schema, and counters; it does not reconstruct every
    /// retained object, so ready time does not grow with retained history.
    #[must_use]
    pub const fn new(limits: MemoryStoreLimits, queue_capacity: NonZeroUsize) -> Self {
        Self {
            limits,
            queue_capacity,
            complete_verify_on_open: false,
        }
    }

    /// Returns the semantic/object limits.
    #[must_use]
    pub const fn limits(self) -> MemoryStoreLimits {
        self.limits
    }

    /// Returns the capacity of each foreground actor lane.
    #[must_use]
    pub const fn queue_capacity(self) -> NonZeroUsize {
        self.queue_capacity
    }
}

/// Local retained-state and accepted-mutation counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteStoreStats {
    sessions: u64,
    branches: u64,
    checkpoints: u64,
    objects: u64,
    object_bytes: u64,
    semantic_mutations: u64,
    tombstones: u64,
    reclaimed_objects: u64,
    reclaimed_object_bytes: u64,
}

/// Bounded persistence-shape evidence for product startup admission.
///
/// This classification is deliberately not semantic-integrity, recovery, or
/// usability authority. [`SqliteSessionStore::open`] admits shape, schema, and
/// counters for attach.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqlitePersistenceShape {
    /// No database exists, or an existing private file has no `SQLite` objects.
    MissingOrFresh,
    /// The bounded header, runtime, pragma, and exact-DDL checks match current.
    Current,
}

/// Size of the main database file and the part of it that is free pages.
///
/// Free pages are space a prune or deletion already reclaimed that the file
/// has not yet returned to the file system; background housekeeping returns them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteFileUsage {
    /// Pages in use or free, in bytes.
    pub file_bytes: u64,
    /// Free pages, in bytes.
    pub free_bytes: u64,
}

/// Retained change-feed records and their encoded bytes across every session,
/// summed from the sessions rows' counters. Informational: feed rows never
/// count against the aggregate retention bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteFeedUsage {
    /// Retained records.
    pub records: u64,
    /// Their encoded bytes.
    pub bytes: u64,
}

/// One successful commit observed by an explicitly instrumented store.
///
/// This is benchmark/test evidence, not semantic or recovery authority.
#[cfg(feature = "perf-instrumentation")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteCommitTrace {
    queue_wait_ns: u64,
    transaction_begin_count: u64,
    transaction_commit_count: u64,
    transaction_elapsed_ns: u64,
    commit_call_elapsed_ns: u64,
    inserted_objects: u64,
    inserted_object_bytes: u64,
}

#[cfg(feature = "perf-instrumentation")]
impl SqliteCommitTrace {
    /// Returns time from successful enqueue until the actor began this job.
    #[must_use]
    pub const fn queue_wait_ns(self) -> u64 {
        self.queue_wait_ns
    }
    /// Returns explicit successful immediate-transaction begin attempts.
    #[must_use]
    pub const fn transaction_begin_count(self) -> u64 {
        self.transaction_begin_count
    }
    /// Returns explicit successful transaction commit calls.
    #[must_use]
    pub const fn transaction_commit_count(self) -> u64 {
        self.transaction_commit_count
    }
    /// Returns time from the immediate-transaction attempt through commit.
    #[must_use]
    pub const fn transaction_elapsed_ns(self) -> u64 {
        self.transaction_elapsed_ns
    }
    /// Returns time spent inside the successful `SQLite` commit call.
    #[must_use]
    pub const fn commit_call_elapsed_ns(self) -> u64 {
        self.commit_call_elapsed_ns
    }
    /// Returns newly inserted content-addressed object rows.
    #[must_use]
    pub const fn inserted_objects(self) -> u64 {
        self.inserted_objects
    }
    /// Returns conservative retained bytes for newly inserted objects/edges.
    #[must_use]
    pub const fn inserted_object_bytes(self) -> u64 {
        self.inserted_object_bytes
    }
}

/// Drainable sink used only by explicitly instrumented store instances.
#[cfg(feature = "perf-instrumentation")]
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct SqliteTraceCollector {
    commits: Arc<Mutex<Vec<SqliteCommitTrace>>>,
}

#[cfg(feature = "perf-instrumentation")]
impl SqliteTraceCollector {
    /// Removes and returns all completed commit traces.
    #[must_use]
    pub fn take_commits(&self) -> Vec<SqliteCommitTrace> {
        self.commits
            .lock()
            .map(|mut commits| std::mem::take(&mut *commits))
            .unwrap_or_default()
    }

    fn record(&self, trace: SqliteCommitTrace) {
        if let Ok(mut commits) = self.commits.lock() {
            commits.push(trace);
        }
    }
}

impl SqliteStoreStats {
    /// Returns cumulative background/explicit object reclamation.
    #[must_use]
    pub const fn reclaimed_objects(self) -> u64 {
        self.reclaimed_objects
    }
    /// Returns cumulative reclaimed payload and edge bytes.
    #[must_use]
    pub const fn reclaimed_object_bytes(self) -> u64 {
        self.reclaimed_object_bytes
    }
    /// Returns retained sessions.
    #[must_use]
    pub const fn sessions(self) -> u64 {
        self.sessions
    }
    /// Returns retained branches.
    #[must_use]
    pub const fn branches(self) -> u64 {
        self.branches
    }
    /// Returns retained checkpoints.
    #[must_use]
    pub const fn checkpoints(self) -> u64 {
        self.checkpoints
    }
    /// Returns unique objects.
    #[must_use]
    pub const fn objects(self) -> u64 {
        self.objects
    }
    /// Returns conservative unique object and edge bytes.
    #[must_use]
    pub const fn object_bytes(self) -> u64 {
        self.object_bytes
    }
    /// Returns accepted semantic mutations.
    #[must_use]
    pub const fn semantic_mutations(self) -> u64 {
        self.semantic_mutations
    }
    /// Returns retained deletion tombstones.
    #[must_use]
    pub const fn tombstones(self) -> u64 {
        self.tombstones
    }
}

/// Durable `SQLite` store using one connection on one bounded blocking actor.
pub struct SqliteSessionStore {
    clock: Arc<dyn LeaseClock>,
    domain: LeaseDomain,
    sender: ActorSender,
    usage_snapshot: Arc<Mutex<Result<StoreUsage, SessionStoreError>>>,
    worker: Arc<Worker>,
    #[cfg(feature = "perf-instrumentation")]
    trace: Option<SqliteTraceCollector>,
}

struct Worker {
    join: Mutex<Option<JoinHandle<()>>>,
}

struct Backend {
    connection: Connection,
    clock: Arc<dyn LeaseClock>,
    domain: LeaseDomain,
    limits: MemoryStoreLimits,
    shutdown_report: Option<DurabilityReport>,
    /// Host-local proof of who is holding which branch right now.
    holders: crate::holder::HolderClaims,
    gc_pending: bool,
    usage_snapshot: Option<Arc<Mutex<Result<StoreUsage, SessionStoreError>>>>,
    vacuum_pending: bool,
    wal_pending: bool,
    checkpoints: CheckpointCache,
    #[cfg(test)]
    checkpoint_decodes: usize,
    #[cfg(test)]
    job_hook: Option<Box<dyn FnMut(ExecutedJob) + Send>>,
}

impl SqliteSessionStore {
    /// Reads one immutable checkpoint without opening a writer or changing leases.
    ///
    /// # Errors
    /// Returns a typed lookup, shape, integrity, or storage failure.
    pub fn inspect_checkpoint(
        path: impl AsRef<Path>,
        checkpoint: CheckpointId,
    ) -> Result<StoredCheckpoint, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        load_checkpoint(&snapshot, checkpoint)
    }

    /// Reads one exact branch without opening a writable runtime or changing its
    /// boot domain, lease, schema, or durable rows. `SQLite` may still update
    /// transient shared-memory reader coordination for a live WAL.
    ///
    /// # Errors
    ///
    /// Returns a typed shape, lookup, integrity, or storage failure.
    pub fn inspect_branch(
        path: impl AsRef<Path>,
        session: SessionRef,
    ) -> Result<LoadedBranch, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        // One snapshot binds the schema check, branch head and all referenced
        // objects. A concurrent writer cannot make these reads span commits.
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        load_branch(&snapshot, session)
    }

    /// Inspects the head row and its addressed diagnostic metadata without history materialization.
    /// This validates only the metadata and obligation paths, not the whole checkpoint graph.
    ///
    /// # Errors
    /// Returns typed lookup, shape, integrity or storage failures.
    pub fn inspect_branch_metadata(
        path: impl AsRef<Path>,
        selector: SessionRef,
    ) -> Result<
        (
            BranchHead,
            plakous_agent::internal::RuntimeCheckpointInspection,
        ),
        SessionStoreError,
    > {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        let session = live_session(&snapshot, selector.session_id())?;
        let branch = branch_row(&snapshot, selector)?.ok_or(SessionStoreError::NotFound)?;
        let reference = branch.head.checkpoint();
        let row = checkpoint_row(&snapshot, reference.checkpoint_id())?
            .ok_or_else(SessionStoreError::corrupt)?;
        let inspection = plakous_agent::internal::RuntimeCheckpointCodec::inspect_metadata(
            reference.checkpoint_id(),
            reference.integrity(),
            reference.root(),
            |address| load_object(&snapshot, address)?.ok_or_else(SessionStoreError::corrupt),
            bundle_error,
        )?;
        let topology = inspection.topology;
        if row.reference != reference
            || row.revision != branch.head.revision()
            || row.selector != selector
            || topology.session_id != selector.session_id()
            || topology.branch_id != selector.branch_id()
            || topology.checkpoint_id != reference.checkpoint_id()
            || topology.branch_revision != branch.head.revision().get()
            || topology.parent_checkpoint_id != row.parent
            || topology.fork != row.lineage
            || topology.lease_fence != row.fence
            || topology.commit_identity != row.commit
            || inspection.durability != row.durability
            || inspection.durability != session.durability()
            || matches!(
                inspection.lifecycle,
                plakous_agent::internal::RuntimeLifecycle::Working { .. }
            ) != (row.kind == RuntimeCheckpointKind::Working)
        {
            return Err(SessionStoreError::corrupt());
        }
        Ok((branch.head, inspection))
    }

    /// Reads one bounded page of a session's change feed after `after`, with
    /// its retained window, over one read-only snapshot: no runtime, lease or
    /// write. The page has the same bounds and retention answer as
    /// [`SessionStore::read_feed`].
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a missing store or an unknown or deleted
    /// session, and a typed shape, integrity, or storage failure otherwise.
    pub fn inspect_feed(
        path: impl AsRef<Path>,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> Result<FeedPage, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        feed::read(&snapshot, session_id, after, max_records)
    }

    /// Reads the change feed's retained records and encoded bytes across
    /// every session from the sessions rows' feed counters, without scanning
    /// the feed or opening a writable runtime. Returns `None` for a missing or
    /// fresh file.
    ///
    /// # Errors
    ///
    /// Returns a typed shape, integrity, or storage failure.
    pub fn inspect_feed_usage(
        path: impl AsRef<Path>,
    ) -> Result<Option<SqliteFeedUsage>, SessionStoreError> {
        let Some(connection) = Self::open_inspection(path.as_ref())? else {
            return Ok(None);
        };
        if classify_connection(&connection)? != SqlitePersistenceShape::Current {
            return Ok(None);
        }
        let (records, bytes) = feed::totals(&connection)?;
        Ok(Some(SqliteFeedUsage { records, bytes }))
    }

    /// Opens one read-only connection to an existing private store file;
    /// `None` for a missing file. Callers admit its shape once, on the snapshot
    /// they read, with [`admit_inspection_snapshot`] or [`classify_connection`].
    fn open_inspection(path: &Path) -> Result<Option<Connection>, SessionStoreError> {
        admit_header_file(path)?;
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(map_io(error)),
        };
        require_private_regular_file(&metadata)?;
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(map_sqlite)?;
        verify_sqlite_runtime(&connection)?;
        connection.busy_timeout(BUSY_TIMEOUT).map_err(map_sqlite)?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")
            .map_err(map_sqlite)?;
        Ok(Some(connection))
    }

    /// Reads aggregate retention usage against the default bounds without
    /// opening a writable runtime. Returns `None` for a missing or fresh file.
    /// Shape and schema are checked; counters are read from `store_meta` as
    /// recorded and are not revalidated against retained rows.
    ///
    /// # Errors
    ///
    /// Returns a typed shape, integrity, or storage failure.
    pub fn inspect_usage(path: impl AsRef<Path>) -> Result<Option<StoreUsage>, SessionStoreError> {
        let Some(connection) = Self::open_inspection(path.as_ref())? else {
            return Ok(None);
        };
        if classify_connection(&connection)? != SqlitePersistenceShape::Current {
            return Ok(None);
        }
        // One store_meta row read: no count validation or object scan, so the
        // doctor stays O(1) in retained history.
        usage(&connection, SqliteStoreOptions::default().limits).map(Some)
    }

    /// Reads the database file's size and free pages without opening a
    /// writable runtime. Returns `None` for a missing or fresh file.
    ///
    /// # Errors
    ///
    /// Returns a typed shape or storage failure.
    pub fn inspect_file_usage(
        path: impl AsRef<Path>,
    ) -> Result<Option<SqliteFileUsage>, SessionStoreError> {
        let Some(connection) = Self::open_inspection(path.as_ref())? else {
            return Ok(None);
        };
        if classify_connection(&connection)? != SqlitePersistenceShape::Current {
            return Ok(None);
        }
        file_usage(&connection).map(Some)
    }

    /// Classifies bounded persistence shape without authorizing semantic use.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe refusal for unsafe files, unsupported schema
    /// versions, or a damaged current shape. Unsupported database and WAL bytes
    /// are preserved; read-only `SQLite` inspection may update temporary SHM coordination.
    pub fn classify_persistence(
        path: impl AsRef<Path>,
    ) -> Result<SqlitePersistenceShape, SessionStoreError> {
        match Self::open_inspection(path.as_ref())? {
            Some(connection) => classify_connection(&connection),
            None => Ok(SqlitePersistenceShape::MissingOrFresh),
        }
    }

    /// Opens or creates a private fresh-schema database.
    ///
    /// This admits bounded shape, exact schema, and counters. It does not
    /// reconstruct every retained object; complete verification is not a
    /// product operation.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe I/O, schema, or corruption error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        Self::open_with_clock_and_options(
            path,
            Arc::new(SystemLeaseClock::new()?),
            SqliteStoreOptions::default(),
        )
    }

    /// Test support: opens after complete object, receipt, and foreign-key
    /// verification inside the writer's admission transaction.
    ///
    /// Not a product operation. It holds the writer lock for an O(store)
    /// reconstruction under the 30-second readiness window, so live writers
    /// would see `BackendBusy` and a large store a readiness timeout.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe I/O, schema, or corruption error.
    #[doc(hidden)]
    pub fn open_verified(path: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        Self::open_with_clock_and_options(
            path,
            Arc::new(SystemLeaseClock::new()?),
            SqliteStoreOptions {
                complete_verify_on_open: true,
                ..SqliteStoreOptions::default()
            },
        )
    }

    /// Opens with an injected clock.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe I/O, schema, or corruption error.
    pub fn open_with_clock(
        path: impl AsRef<Path>,
        clock: Arc<dyn LeaseClock>,
    ) -> Result<Self, SessionStoreError> {
        Self::open_with_clock_and_options(path, clock, SqliteStoreOptions::default())
    }

    /// Opens with explicit actor and retained-state bounds.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe I/O, schema, or corruption error.
    pub fn open_with_clock_and_options(
        path: impl AsRef<Path>,
        clock: Arc<dyn LeaseClock>,
        options: SqliteStoreOptions,
    ) -> Result<Self, SessionStoreError> {
        Self::open_inner(
            path,
            clock,
            options,
            #[cfg(feature = "perf-instrumentation")]
            None,
        )
    }

    /// Opens a store with commit-phase tracing for offline benchmarks/tests.
    ///
    /// Normal store construction does not allocate or record these traces.
    #[cfg(feature = "perf-instrumentation")]
    #[doc(hidden)]
    pub fn open_instrumented(
        path: impl AsRef<Path>,
    ) -> Result<(Self, SqliteTraceCollector), SessionStoreError> {
        let trace = SqliteTraceCollector::default();
        let store = Self::open_inner(
            path,
            Arc::new(SystemLeaseClock::new()?),
            SqliteStoreOptions::default(),
            Some(trace.clone()),
        )?;
        Ok((store, trace))
    }

    fn open_inner(
        path: impl AsRef<Path>,
        clock: Arc<dyn LeaseClock>,
        options: SqliteStoreOptions,
        #[cfg(feature = "perf-instrumentation")] trace: Option<SqliteTraceCollector>,
    ) -> Result<Self, SessionStoreError> {
        let domain = clock.now()?.domain();
        let path = path.as_ref().to_path_buf();
        let progress = Arc::new(StartupProgress(Mutex::new(StartupStage::Header)));
        admit_existing_header(&path, Some(domain))
            .map_err(|error| progress.failure("preparation", error).report())?;
        progress.set(StartupStage::PrivateFile);
        prepare_private_file(&path)
            .map_err(|error| progress.failure("preparation", error).report())?;
        progress.set(StartupStage::Actor);
        let (sender, receiver) = actor_queue(options.queue_capacity.get());
        let usage_snapshot = Arc::new(Mutex::new(Err(SessionStoreError::BackendUnavailable)));
        let worker_usage = Arc::clone(&usage_snapshot);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let authority_clock = Arc::clone(&clock);
        let worker_progress = Arc::clone(&progress);
        let join = thread::Builder::new()
            .name("plakous-sqlite-store".into())
            .spawn(
                move || match Backend::open(&path, clock, domain, options, &worker_progress) {
                    Ok(mut backend) => {
                        backend.usage_snapshot = Some(Arc::clone(&worker_usage));
                        *worker_usage
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            usage(&backend.connection, backend.limits);
                        worker_progress.set(StartupStage::Ready);
                        if ready_tx.send(Ok(())).is_err() {
                            return;
                        }
                        receiver.run(&mut backend);
                        *worker_usage
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Err(SessionStoreError::BackendUnavailable);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                },
            )
            .map_err(|_| {
                progress
                    .failure("spawn", SessionStoreError::BackendUnavailable)
                    .report()
            })?;
        let join =
            wait_for_actor_or_join(ready_rx, sender.clone(), join, STARTUP_TIMEOUT, &progress)?;
        Ok(Self {
            clock: authority_clock,
            domain,
            sender,
            usage_snapshot,
            worker: Arc::new(Worker {
                join: Mutex::new(Some(join)),
            }),
            #[cfg(feature = "perf-instrumentation")]
            trace,
        })
    }

    /// Reads local counters without changing authority.
    pub fn stats(&self) -> StoreFuture<'_, SqliteStoreStats> {
        self.dispatch_read("stats", |backend| {
            backend.require_open()?;
            read_stats(&backend.connection)
        })
    }

    fn dispatch<T, F>(&self, label: &'static str, operation: F) -> StoreFuture<'_, T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Backend) -> Result<T, SessionStoreError> + Send + 'static,
    {
        self.dispatch_lane(Lane::Critical, label, operation)
    }

    fn dispatch_read<T, F>(&self, label: &'static str, operation: F) -> StoreFuture<'_, T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Backend) -> Result<T, SessionStoreError> + Send + 'static,
    {
        self.dispatch_lane(Lane::Read, label, operation)
    }

    fn dispatch_lane<T, F>(
        &self,
        lane: Lane,
        label: &'static str,
        operation: F,
    ) -> StoreFuture<'_, T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Backend) -> Result<T, SessionStoreError> + Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let snapshot = Arc::clone(&self.usage_snapshot);
        let activity = Arc::clone(&self.sender.activity);
        let job: Job = Box::new(move |backend| {
            let started = activity.start(label);
            #[cfg(test)]
            if let Some(hook) = &mut backend.job_hook {
                hook(match lane {
                    Lane::Critical => ExecutedJob::Critical,
                    Lane::Read => ExecutedJob::Read,
                });
            }
            #[cfg(feature = "commit-trace")]
            let settle = crate::commit_trace::SettleGuard;
            let result = operation(backend);
            if matches!(lane, Lane::Critical) {
                *snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = backend
                    .require_open()
                    .and_then(|()| usage(&backend.connection, backend.limits));
            }
            #[cfg(feature = "commit-trace")]
            drop(settle);
            activity.finish(lane.label(), label, started);
            let _ = reply.send(result);
        });
        let queued = match lane {
            Lane::Critical => {
                let queued = self.sender.send(lane, job);
                Box::pin(async move { queued }) as StoreFuture<'_, ()>
            }
            Lane::Read => self.sender.send_read(job),
        };
        Box::pin(async move {
            queued.await?;
            response.await.map_err(|_| {
                actor_lost(lane, "reply_dropped");
                SessionStoreError::BackendUnavailable
            })?
        })
    }
}

/// Admits identity, schema version, shape and counters inside one read-only
/// inspection snapshot.
/// Bounded shape of one read-only connection: fresh, or exactly this build's
/// application, version, shape pragmas and schema.
fn classify_connection(c: &Connection) -> Result<SqlitePersistenceShape, SessionStoreError> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(map_sqlite)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite)?;
    let objects: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(map_sqlite)?;
    if app == 0 && version == 0 && objects == 0 {
        return Ok(SqlitePersistenceShape::MissingOrFresh);
    }
    if app != APPLICATION_ID {
        return Err(SessionStoreError::corrupt());
    }
    if version != SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchema);
    }
    verify_shape_pragmas(c)?;
    verify_exact_schema(c)?;
    Ok(SqlitePersistenceShape::Current)
}

/// Admits one inspection snapshot: a current shape and a bound clock domain.
///
/// Retained-row counts are not revalidated. They guard the writer's admission
/// accounting, which inspection never touches, and counting them is O(store)
/// on every read. A fresh file has nothing to inspect.
fn admit_inspection_snapshot(snapshot: &Connection) -> Result<(), SessionStoreError> {
    if classify_connection(snapshot)? != SqlitePersistenceShape::Current {
        return Err(SessionStoreError::NotFound);
    }
    bound_domain(snapshot)?;
    Ok(())
}

/// The file half of the header preflight: refuses a non-regular file without
/// blocking on it and a foreign or unsupported main-file header. Returns whether
/// the file has content whose committed identity still needs a connection.
fn admit_header_file(path: &Path) -> Result<bool, SessionStoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Reject non-regular descriptors below without blocking on a FIFO open,
        // including a FIFO swapped into the path immediately before this call.
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        );
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(map_io(error)),
    };
    let metadata = file.metadata().map_err(map_io)?;
    if !metadata.is_file() {
        return Err(SessionStoreError::BackendUnavailable);
    }
    if metadata.len() == 0 {
        return Ok(false);
    }
    let mut header = [0_u8; 72];
    file.read_exact(&mut header)
        .map_err(|_| SessionStoreError::corrupt())?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(SessionStoreError::corrupt());
    }
    let version = i64::from(u32::from_be_bytes(
        header[60..64]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
    ));
    let application = i64::from(u32::from_be_bytes(
        header[68..72]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
    ));
    if application != 0 || version != 0 {
        if application != APPLICATION_ID {
            return Err(SessionStoreError::corrupt());
        }
        if version != SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedSchema);
        }
    }
    Ok(true)
}

/// Refuse unsupported versions before opening a writer or adjusting permissions.
/// Read-only WAL inspection may reconstruct or update temporary SHM coordination,
/// but preserves the database and WAL bytes. A supplied clock domain also
/// refuses a foreign same-boot scope before any writer can alter journal state.
/// The authoritative transaction repeats the domain check after this preflight.
fn admit_existing_header(
    path: &Path,
    domain: Option<LeaseDomain>,
) -> Result<(), SessionStoreError> {
    if !admit_header_file(path)? {
        return Ok(());
    }
    // A WAL can carry a newer header than the main file. Read its committed
    // identity without opening a writer or checkpointing it into the main file.
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(map_sqlite)?;
    let application: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(map_sqlite)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite)?;
    if application == 0 && version == 0 {
        let objects: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;
        return if objects == 0 {
            Ok(())
        } else {
            Err(SessionStoreError::corrupt())
        };
    }
    if application != APPLICATION_ID {
        return Err(SessionStoreError::corrupt());
    }
    if version != SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchema);
    }
    if let Some(domain) = domain {
        connection
            .execute_batch("PRAGMA trusted_schema=OFF;")
            .map_err(map_sqlite)?;
        verify_exact_schema(&connection)?;
        let bound = bound_domain(&connection)?;
        if bound.epoch() == domain.epoch() && bound != domain {
            return Err(SessionStoreError::LeaseDomainMismatch);
        }
    }
    Ok(())
}

fn wait_for_actor(
    ready: &mpsc::Receiver<Result<(), SessionStoreError>>,
    timeout: Duration,
    progress: &StartupProgress,
) -> Result<(), StartupFailure> {
    match ready.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(progress.failure("backend", error)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(progress.failure("readiness_timeout", SessionStoreError::BackendUnavailable))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(progress.failure(
            "readiness_disconnected",
            SessionStoreError::BackendUnavailable,
        )),
    }
}

fn wait_for_actor_or_join<S>(
    ready: mpsc::Receiver<Result<(), SessionStoreError>>,
    sender: S,
    join: JoinHandle<()>,
    timeout: Duration,
    progress: &StartupProgress,
) -> Result<JoinHandle<()>, SessionStoreError> {
    match wait_for_actor(&ready, timeout, progress) {
        Ok(()) => Ok(join),
        Err(failure) => {
            // Report the snapshot before joining: late actor progress cannot
            // overwrite the failing stage, and a stalled join stays diagnosable.
            let error = failure.report();
            drop(ready);
            drop(sender);
            let _ = join.join();
            Err(error)
        }
    }
}

#[cfg(feature = "perf-instrumentation")]
fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Clone for SqliteSessionStore {
    fn clone(&self) -> Self {
        Self {
            clock: Arc::clone(&self.clock),
            domain: self.domain,
            sender: self.sender.clone(),
            usage_snapshot: Arc::clone(&self.usage_snapshot),
            worker: Arc::clone(&self.worker),
            #[cfg(feature = "perf-instrumentation")]
            trace: self.trace.clone(),
        }
    }
}

impl fmt::Debug for SqliteSessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteSessionStore").finish_non_exhaustive()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Ok(slot) = self.join.get_mut()
            && let Some(join) = slot.take()
        {
            let _ = join.join();
        }
    }
}

impl Backend {
    fn open(
        path: &Path,
        clock: Arc<dyn LeaseClock>,
        domain: LeaseDomain,
        options: SqliteStoreOptions,
        progress: &StartupProgress,
    ) -> Result<Self, SessionStoreError> {
        let limits = options.limits;
        progress.set(StartupStage::Connection);
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(map_sqlite)?;
        progress.set(StartupStage::Runtime);
        verify_sqlite_runtime(&connection)?;
        progress.set(StartupStage::Configuration);
        connection.busy_timeout(BUSY_TIMEOUT).map_err(map_sqlite)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
             PRAGMA trusted_schema=OFF;
             PRAGMA auto_vacuum=INCREMENTAL;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA cache_size=-65536;
             PRAGMA journal_size_limit=67108864;",
            )
            .map_err(map_sqlite)?;
        // Schema admission and boot-domain replacement share one writer snapshot.
        // A failed check leaves both existing authority and fresh creation intact.
        let admission = Transaction::new_unchecked(&connection, TransactionBehavior::Immediate)
            .map_err(map_sqlite)?;
        progress.set(StartupStage::Schema);
        initialize_schema(&admission, domain)?;
        progress.set(StartupStage::Admission);
        if options.complete_verify_on_open {
            verify_database(&admission)?;
        } else {
            admit_open(&admission)?;
        }
        sample_clock(clock.as_ref(), domain)?;
        bind_domain(&admission, domain)?;
        admission.commit().map_err(map_sqlite)?;
        let vacuum_pending = connection
            .pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))
            .map_err(map_sqlite)?
            > 0;
        #[cfg(feature = "commit-trace")]
        let tracing = crate::commit_trace::install(&connection, "runtime").map_err(map_sqlite)?;
        #[cfg(all(test, feature = "commit-trace"))]
        if !tracing {
            install_test_commit_hook(&connection);
        }
        #[cfg(all(test, not(feature = "commit-trace")))]
        install_test_commit_hook(&connection);
        #[cfg(all(not(test), feature = "commit-trace"))]
        let _ = tracing;
        let gc_pending = gc::pending(&connection)?;
        Ok(Self {
            connection,
            clock,
            domain,
            limits,
            shutdown_report: None,
            holders: crate::holder::HolderClaims::beside(path),
            gc_pending,
            usage_snapshot: None,
            vacuum_pending,
            wal_pending: vacuum_pending,
            checkpoints: CheckpointCache::new(CHECKPOINT_CACHE_BYTES),
            #[cfg(test)]
            checkpoint_decodes: 0,
            #[cfg(test)]
            job_hook: None,
        })
    }

    fn require_open(&self) -> Result<(), SessionStoreError> {
        if self.shutdown_report.is_some() {
            Err(SessionStoreError::Shutdown)
        } else {
            Ok(())
        }
    }

    fn load_branch(&mut self, selector: SessionRef) -> Result<LoadedBranch, SessionStoreError> {
        let session = live_session(&self.connection, selector.session_id())?;
        let (row, checkpoint_row) = branch_row_with_checkpoint(&self.connection, selector)?
            .ok_or(SessionStoreError::NotFound)?;
        let checkpoint =
            self.load_checkpoint_row(row.head.checkpoint().checkpoint_id(), &checkpoint_row)?;
        if checkpoint.reference() != row.head.checkpoint() {
            return Err(SessionStoreError::corrupt());
        }
        with_loaded_horizon(
            &self.connection,
            selector,
            LoadedBranch::from_store(session, row.head, checkpoint),
        )
    }

    fn load_checkpoint(&mut self, id: CheckpointId) -> Result<StoredCheckpoint, SessionStoreError> {
        // The cache never substitutes for relational authority or its integrity seal.
        let Some(row) = checkpoint_row(&self.connection, id)? else {
            self.checkpoints.remove(id);
            return Err(if pruned_checkpoint(&self.connection, id)? {
                SessionStoreError::Pruned
            } else {
                SessionStoreError::NotFound
            });
        };
        self.load_checkpoint_row(id, &row)
    }

    fn load_checkpoint_row(
        &mut self,
        id: CheckpointId,
        row: &CheckpointRow,
    ) -> Result<StoredCheckpoint, SessionStoreError> {
        if let Some(checkpoint) = self.checkpoints.get(id) {
            if checkpoint.reference() != row.reference {
                return Err(SessionStoreError::corrupt());
            }
            return Ok(checkpoint);
        }
        #[cfg(test)]
        {
            self.checkpoint_decodes += 1;
        }
        let checkpoint = load_checkpoint_contents(&self.connection, id, row)?;
        self.checkpoints.insert(checkpoint.clone());
        Ok(checkpoint)
    }

    fn retry_after_reclaim<T>(
        &mut self,
        mut operation: impl FnMut(&mut Self) -> Result<T, SessionStoreError>,
    ) -> Result<T, SessionStoreError> {
        match operation(self) {
            Err(SessionStoreError::Capacity(_)) => {
                self.reclaim(StoreReclaimMode::Bounded)?;
                operation(self)
            }
            result => result,
        }
    }

    fn publish_usage(&self) {
        if let Some(snapshot) = &self.usage_snapshot {
            *snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self
                .require_open()
                .and_then(|()| usage(&self.connection, self.limits));
        }
    }

    fn reclaim(&mut self, mode: StoreReclaimMode) -> Result<StoreReclaimReport, SessionStoreError> {
        self.require_open()?;
        let mut objects = 0;
        let mut bytes = 0;
        loop {
            let report = gc::step(&mut self.connection, 256, 8 * 1024 * 1024)?;
            self.gc_pending = report.pending();
            objects += report.objects();
            bytes += report.object_bytes();
            if report.objects() > 0 {
                self.vacuum_pending = true;
                self.wal_pending = true;
            }
            self.publish_usage();
            if mode == StoreReclaimMode::Bounded || !report.pending() {
                return Ok(StoreReclaimReport::from_store(
                    objects,
                    bytes,
                    report.pending(),
                ));
            }
        }
    }

    fn background_step(&mut self) -> bool {
        if self.shutdown_report.is_some()
            || (!self.gc_pending && !self.vacuum_pending && !self.wal_pending)
        {
            return false;
        }
        #[cfg(test)]
        if let Some(hook) = &mut self.job_hook {
            hook(ExecutedJob::Background);
        }
        if self.gc_pending {
            self.gc_pending = self
                .reclaim(StoreReclaimMode::Bounded)
                .is_ok_and(StoreReclaimReport::pending);
        } else if self.wal_pending {
            self.wal_pending = false;
            let _ = checkpoint_wal(&self.connection, false);
        } else {
            let result = incremental_vacuum(&self.connection, BACKGROUND_VACUUM_PAGES);
            self.vacuum_pending = result.is_ok()
                && self
                    .connection
                    .pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0))
                    .is_ok_and(|pages| pages > 0);
            self.wal_pending = true;
        }
        true
    }
}

fn domain_bytes(domain: LeaseDomain) -> Vec<u8> {
    [domain.epoch().as_slice(), domain.scope().as_slice()].concat()
}

fn decode_domain(bytes: &[u8]) -> Result<LeaseDomain, SessionStoreError> {
    let bytes: &[u8; 48] = bytes.try_into().map_err(|_| SessionStoreError::corrupt())?;
    Ok(LeaseDomain::new(
        bytes[..16]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
        bytes[16..]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
    ))
}

fn bound_domain(c: &Connection) -> Result<LeaseDomain, SessionStoreError> {
    let bytes = c
        .query_row(
            "SELECT lease_domain FROM store_meta WHERE id=1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(map_sqlite)?;
    decode_domain(&bytes)
}

fn require_bound_domain(c: &Connection, domain: LeaseDomain) -> Result<(), SessionStoreError> {
    if bound_domain(c)? != domain {
        return Err(SessionStoreError::LeaseDomainMismatch);
    }
    Ok(())
}

fn bind_domain(c: &Connection, domain: LeaseDomain) -> Result<(), SessionStoreError> {
    let previous = bound_domain(c)?;
    if previous == domain {
        return Ok(());
    }
    if previous.epoch() == domain.epoch() {
        return Err(SessionStoreError::LeaseDomainMismatch);
    }
    // Only construction may retire a prior boot, under the same-host contract.
    exactly_one(
        c.execute(
            "UPDATE store_meta SET lease_domain=?1 WHERE id=1",
            [domain_bytes(domain)],
        )
        .map_err(map_sqlite)?,
    )
}

fn verify_sqlite_runtime(c: &Connection) -> Result<(), SessionStoreError> {
    let (version, source_id): (String, String) = c
        .query_row("SELECT sqlite_version(), sqlite_source_id()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(map_sqlite)?;
    if version != BUNDLED_SQLITE_VERSION || source_id != BUNDLED_SQLITE_SOURCE_ID {
        return Err(SessionStoreError::BackendUnavailable);
    }
    for option in BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS {
        let present: i64 = c
            .query_row("SELECT sqlite_compileoption_used(?1)", [option], |row| {
                row.get(0)
            })
            .map_err(map_sqlite)?;
        if present != 1 {
            return Err(SessionStoreError::BackendUnavailable);
        }
    }
    Ok(())
}

impl SessionStore for SqliteSessionStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        sample_clock(self.clock.as_ref(), self.domain)
    }

    fn create(&self, r: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        self.dispatch("create", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("create");
            let receipt = b.retry_after_reclaim(|b| create(b, &r))?;
            feed::trim(b, r.session().session_id());
            Ok(receipt)
        })
    }
    fn recover_create(&self, r: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        self.dispatch("recover_create", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("recover_create");
            recover_create(&b.connection, &r)
        })
    }
    fn load_branch(&self, s: SessionRef) -> StoreFuture<'_, LoadedBranch> {
        self.dispatch_read("load_branch", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("load_branch");
            b.require_open()?;
            b.load_branch(s)
        })
    }
    fn load_checkpoint(&self, c: CheckpointId) -> StoreFuture<'_, StoredCheckpoint> {
        self.dispatch_read("load_checkpoint", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("load_checkpoint");
            b.load_checkpoint(c)
        })
    }
    fn acquire(&self, r: AcquireBranch) -> StoreFuture<'_, BranchLease> {
        self.dispatch("acquire", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("acquire");
            acquire(b, r)
        })
    }
    fn renew(&self, l: BranchLease) -> StoreFuture<'_, BranchLease> {
        self.dispatch("renew", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("renew");
            renew(b, &l)
        })
    }
    fn revive(&self, l: BranchLease) -> StoreFuture<'_, BranchLease> {
        self.dispatch("revive", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("revive");
            revive(b, &l)
        })
    }
    fn release(&self, l: BranchLease) -> StoreFuture<'_, ()> {
        self.dispatch("release", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("release");
            release(b, &l)?;
            feed::trim(b, l.session().session_id());
            Ok(())
        })
    }
    fn release_many(&self, leases: Vec<BranchLease>) -> StoreFuture<'_, ()> {
        self.dispatch("release_many", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("release_many");
            release_many(b, &leases)
        })
    }
    fn commit(&self, r: CommitCheckpoint) -> StoreFuture<'_, CommitReceipt> {
        #[cfg(feature = "perf-instrumentation")]
        if let Some(collector) = self.trace.clone() {
            let queued_at = Instant::now();
            return self.dispatch("commit", move |b| {
                #[cfg(feature = "commit-trace")]
                let _label = crate::commit_trace::label("commit");
                let queue_wait_ns = elapsed_ns(queued_at);
                let (receipt, execution) = b.retry_after_reclaim(|b| commit_observed(b, &r))?;
                feed::trim(b, r.expected().session().session_id());
                collector.record(SqliteCommitTrace {
                    queue_wait_ns,
                    transaction_begin_count: execution.transaction_begin_count,
                    transaction_commit_count: execution.transaction_commit_count,
                    transaction_elapsed_ns: execution.transaction_elapsed_ns,
                    commit_call_elapsed_ns: execution.commit_call_elapsed_ns,
                    inserted_objects: execution.inserted.objects,
                    inserted_object_bytes: execution.inserted.bytes,
                });
                Ok(receipt)
            });
        }
        self.dispatch("commit", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("commit");
            let receipt = b.retry_after_reclaim(|b| commit(b, &r))?;
            feed::trim(b, r.expected().session().session_id());
            Ok(receipt)
        })
    }
    fn recover_commit(&self, c: CheckpointId) -> StoreFuture<'_, CommittedCheckpoint> {
        self.dispatch("recover_commit", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("recover_commit");
            recover_commit(&b.connection, c)
        })
    }
    fn fork(&self, r: ForkBranch) -> StoreFuture<'_, ForkReceipt> {
        self.dispatch("fork", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("fork");
            let receipt = b.retry_after_reclaim(|b| fork(b, &r))?;
            feed::trim(b, r.source().session().session_id());
            Ok(receipt)
        })
    }
    fn recover_fork(&self, r: RecoverFork) -> StoreFuture<'_, ForkReceipt> {
        self.dispatch("recover_fork", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("recover_fork");
            recover_fork(b, &r)
        })
    }
    fn list(&self, q: SessionQuery) -> StoreFuture<'_, Page<SessionSummary>> {
        self.dispatch_read("list", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("list");
            b.require_open()?;
            list(&b.connection, b.limits, q)
        })
    }
    fn usage(&self) -> StoreFuture<'_, StoreUsage> {
        let snapshot = *self
            .usage_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Box::pin(async move { snapshot })
    }
    fn reclaim(&self, mode: StoreReclaimMode) -> StoreFuture<'_, StoreReclaimReport> {
        self.dispatch("reclaim", move |b| {
            b.checkpoints.clear();
            b.reclaim(mode)
        })
    }
    fn archive(&self, r: ArchiveSession) -> StoreFuture<'_, ArchiveReceipt> {
        self.dispatch("archive", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("archive");
            let receipt = archive(b, r)?;
            feed::trim(b, r.session_id());
            Ok(receipt)
        })
    }
    fn delete(&self, r: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        self.dispatch("delete", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("delete");
            let result = delete(b, r);
            b.checkpoints.clear();
            result
        })
    }
    fn recover_delete(&self, r: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        self.dispatch("recover_delete", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("recover_delete");
            load_delete(&b.connection, r)
        })
    }
    fn prune(&self, r: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
        self.dispatch("prune", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("prune");
            let receipt = prune(b, &r);
            b.checkpoints.clear();
            let receipt = receipt?;
            feed::trim(b, r.expected().session().session_id());
            Ok(receipt)
        })
    }
    fn read_feed(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> StoreFuture<'_, FeedPage> {
        self.dispatch_read("read_feed", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("read_feed");
            let snapshot = b.connection.transaction().map_err(map_sqlite)?;
            feed::read(&snapshot, session_id, after, max_records)
        })
    }
    fn feed_tail(&self, session_id: SessionId) -> StoreFuture<'_, FeedTail> {
        self.dispatch_read("feed_tail", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("feed_tail");
            feed::tail(&b.connection, session_id)
        })
    }
    fn flush(&self) -> StoreFuture<'_, DurabilityReport> {
        self.dispatch("flush", |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("flush");
            b.require_open()?;
            checkpoint_wal(&b.connection, false)?;
            Ok(report())
        })
    }
    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
        self.dispatch("shutdown", |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("shutdown");
            if let Some(r) = b.shutdown_report {
                return Ok(r);
            }
            checkpoint_wal(&b.connection, true)?;
            let r = report();
            b.shutdown_report = Some(r);
            Ok(r)
        })
    }
}
fn initialize_schema(c: &Connection, domain: LeaseDomain) -> Result<(), SessionStoreError> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(map_sqlite)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(map_sqlite)?;
    let tables: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .map_err(map_sqlite)?;
    if app == 0 && version == 0 && tables == 0 {
        c.execute_batch(SCHEMA).map_err(map_sqlite)?;
        c.execute(
            "UPDATE store_meta SET lease_domain=?1 WHERE id=1",
            [domain_bytes(domain)],
        )
        .map_err(map_sqlite)?;
        c.pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(map_sqlite)?;
        c.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(map_sqlite)?;
        return Ok(());
    }
    if app != APPLICATION_ID {
        return Err(SessionStoreError::corrupt());
    }
    if version != SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchema);
    }
    Ok(())
}

const SCHEMA: &str = r"
CREATE TABLE store_meta (
 id INTEGER PRIMARY KEY CHECK(id=1), sessions INTEGER NOT NULL CHECK(sessions>=0),
 branches INTEGER NOT NULL CHECK(branches>=0), checkpoints INTEGER NOT NULL CHECK(checkpoints>=0),
 objects INTEGER NOT NULL CHECK(objects>=0), object_bytes INTEGER NOT NULL CHECK(object_bytes>=0),
 semantic_mutations INTEGER NOT NULL CHECK(semantic_mutations>=0),
 tombstones INTEGER NOT NULL CHECK(tombstones>=0),
 reclaimed_objects INTEGER NOT NULL CHECK(reclaimed_objects>=0),
 reclaimed_object_bytes INTEGER NOT NULL CHECK(reclaimed_object_bytes>=0),
 lease_domain BLOB NOT NULL CHECK(length(lease_domain)=48));
INSERT INTO store_meta VALUES(1,0,0,0,0,0,0,0,0,0,zeroblob(48));
CREATE TABLE runtime_objects (
 id INTEGER PRIMARY KEY,
 kind INTEGER NOT NULL CHECK(kind BETWEEN 1 AND 19),
 codec INTEGER NOT NULL CHECK(codec=27), digest BLOB NOT NULL CHECK(length(digest)=32),
 byte_len INTEGER NOT NULL CHECK(byte_len>=0), UNIQUE(kind,codec,digest));
CREATE TABLE runtime_object_bytes (
 id INTEGER PRIMARY KEY REFERENCES runtime_objects(id), bytes BLOB NOT NULL);
CREATE TABLE gc_candidates (
 kind INTEGER NOT NULL, codec INTEGER NOT NULL, digest BLOB NOT NULL,
 PRIMARY KEY(kind,codec,digest),
 FOREIGN KEY(kind,codec,digest) REFERENCES runtime_objects(kind,codec,digest)) WITHOUT ROWID;
CREATE TABLE runtime_object_edges (
 parent_kind INTEGER NOT NULL, parent_codec INTEGER NOT NULL, parent_digest BLOB NOT NULL,
 position INTEGER NOT NULL CHECK(position>=0), child_kind INTEGER NOT NULL,
 child_codec INTEGER NOT NULL, child_digest BLOB NOT NULL,
 PRIMARY KEY(parent_kind,parent_codec,parent_digest,position),
 FOREIGN KEY(parent_kind,parent_codec,parent_digest) REFERENCES runtime_objects(kind,codec,digest),
 FOREIGN KEY(child_kind,child_codec,child_digest) REFERENCES runtime_objects(kind,codec,digest))
 WITHOUT ROWID;
CREATE TABLE checkpoints (
 checkpoint_id TEXT PRIMARY KEY, root_kind INTEGER NOT NULL, root_codec INTEGER NOT NULL,
 root_digest BLOB NOT NULL, integrity_version INTEGER NOT NULL, integrity BLOB NOT NULL,
 commit_version INTEGER NOT NULL, commit_digest BLOB NOT NULL,
 base_kind INTEGER, base_codec INTEGER, base_digest BLOB,
 session_id TEXT NOT NULL, branch_id TEXT NOT NULL, revision BLOB NOT NULL,
 parent_id TEXT, fork_session TEXT, fork_branch TEXT, fork_checkpoint TEXT, fork_revision BLOB,
 fence BLOB NOT NULL, kind INTEGER NOT NULL CHECK(kind IN(1,2)),
 durability INTEGER NOT NULL CHECK(durability IN(1,2)),
 semantic_key BLOB NOT NULL,
 validation_seal BLOB CHECK(validation_seal IS NULL OR length(validation_seal)=32),
 UNIQUE(root_kind,root_codec,root_digest),
 FOREIGN KEY(root_kind,root_codec,root_digest) REFERENCES runtime_objects(kind,codec,digest),
 FOREIGN KEY(fork_checkpoint) REFERENCES checkpoints(checkpoint_id),
 CHECK((base_kind IS NULL)=(base_codec IS NULL) AND (base_kind IS NULL)=(base_digest IS NULL)),
 CHECK((fork_session IS NULL)=(fork_branch IS NULL)
   AND (fork_session IS NULL)=(fork_checkpoint IS NULL)
   AND (fork_session IS NULL)=(fork_revision IS NULL)));
CREATE TABLE sessions (
 session_id TEXT PRIMARY KEY, initial_branch TEXT NOT NULL, revision BLOB NOT NULL,
 created_s BLOB NOT NULL, created_ns INTEGER NOT NULL CHECK(created_ns BETWEEN 0 AND 999999999),
 durability INTEGER NOT NULL CHECK(durability IN(1,2)), archived INTEGER NOT NULL CHECK(archived IN(0,1)),
 feed_tail INTEGER NOT NULL CHECK(feed_tail>=0),
 feed_retained_from INTEGER NOT NULL CHECK(feed_retained_from>=1),
 feed_bytes INTEGER NOT NULL CHECK(feed_bytes>=0));
CREATE TABLE branches (
 session_id TEXT NOT NULL, branch_id TEXT NOT NULL, revision BLOB NOT NULL,
 checkpoint_id TEXT NOT NULL, last_fence BLOB NOT NULL,
 fork_session TEXT, fork_branch TEXT, fork_checkpoint TEXT, fork_revision BLOB,
 active_owner TEXT, active_fence BLOB, active_time BLOB CHECK(active_time IS NULL OR length(active_time)=56), active_ns INTEGER, active_duration BLOB,
 previous_time BLOB CHECK(previous_time IS NULL OR length(previous_time)=56), previous_ns INTEGER,
 released_owner TEXT, released_fence BLOB, released_time BLOB CHECK(released_time IS NULL OR length(released_time)=56), released_ns INTEGER, released_duration BLOB,
 PRIMARY KEY(session_id,branch_id), FOREIGN KEY(session_id) REFERENCES sessions(session_id),
 FOREIGN KEY(checkpoint_id) REFERENCES checkpoints(checkpoint_id),
 FOREIGN KEY(fork_checkpoint) REFERENCES checkpoints(checkpoint_id),
 CHECK((fork_session IS NULL)=(fork_branch IS NULL)
   AND (fork_session IS NULL)=(fork_checkpoint IS NULL)
   AND (fork_session IS NULL)=(fork_revision IS NULL))) WITHOUT ROWID;
CREATE TABLE create_receipts (
 session_id TEXT PRIMARY KEY, branch_id TEXT NOT NULL, created_s BLOB NOT NULL,
 created_ns INTEGER NOT NULL, durability INTEGER NOT NULL, owner TEXT NOT NULL,
 duration BLOB NOT NULL, checkpoint_id TEXT NOT NULL, fence BLOB NOT NULL,
 expires_time BLOB NOT NULL CHECK(length(expires_time)=56), expires_ns INTEGER NOT NULL,
 receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 feed_seq INTEGER NOT NULL CHECK(feed_seq>=1),
 FOREIGN KEY(session_id,branch_id) REFERENCES branches(session_id,branch_id),
 FOREIGN KEY(checkpoint_id) REFERENCES checkpoints(checkpoint_id));
CREATE TABLE commit_receipts (
 checkpoint_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, branch_id TEXT NOT NULL,
 owner TEXT NOT NULL, fence BLOB NOT NULL, expires_time BLOB NOT NULL CHECK(length(expires_time)=56), expires_ns INTEGER NOT NULL,
 duration BLOB NOT NULL, expected_revision BLOB NOT NULL, expected_checkpoint TEXT NOT NULL,
 head_revision BLOB NOT NULL, receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 feed_seq INTEGER NOT NULL CHECK(feed_seq>=1),
 FOREIGN KEY(checkpoint_id) REFERENCES checkpoints(checkpoint_id),
 FOREIGN KEY(expected_checkpoint) REFERENCES checkpoints(checkpoint_id));
CREATE TABLE fork_receipts (
 workflow_id BLOB PRIMARY KEY CHECK(length(workflow_id)=32),
 target_session TEXT NOT NULL, target_branch TEXT NOT NULL,
 source_session TEXT NOT NULL, source_branch TEXT NOT NULL,
 expected_session_revision BLOB NOT NULL, source_revision BLOB NOT NULL,
 semantic_request_version INTEGER NOT NULL CHECK(semantic_request_version BETWEEN 1 AND 4294967295),
 semantic_request BLOB NOT NULL CHECK(length(semantic_request)=32),
 source_checkpoint TEXT NOT NULL, transition INTEGER NOT NULL CHECK(transition IN(1,2)),
 target_checkpoint TEXT NOT NULL, target_owner TEXT NOT NULL, target_duration BLOB NOT NULL,
 target_fence BLOB NOT NULL,
 target_expires_time BLOB NOT NULL CHECK(length(target_expires_time)=56), target_expires_ns INTEGER NOT NULL, initial_branch TEXT NOT NULL,
 created_s BLOB NOT NULL, created_ns INTEGER NOT NULL, durability INTEGER NOT NULL,
 previous_session_revision BLOB NOT NULL, session_revision BLOB NOT NULL,
 receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 feed_seq INTEGER NOT NULL CHECK(feed_seq>=1),
 UNIQUE(target_session,target_branch),
 FOREIGN KEY(target_session,target_branch) REFERENCES branches(session_id,branch_id),
 FOREIGN KEY(source_checkpoint) REFERENCES checkpoints(checkpoint_id),
 FOREIGN KEY(target_checkpoint) REFERENCES checkpoints(checkpoint_id)) WITHOUT ROWID;
CREATE TABLE archive_receipts (
 session_id TEXT PRIMARY KEY, initial_branch TEXT NOT NULL, created_s BLOB NOT NULL,
 created_ns INTEGER NOT NULL, durability INTEGER NOT NULL, expected_revision BLOB NOT NULL,
 session_revision BLOB NOT NULL, receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 FOREIGN KEY(session_id) REFERENCES sessions(session_id));
CREATE TABLE delete_tombstones (
 session_id TEXT PRIMARY KEY, initial_branch TEXT NOT NULL, created_s BLOB NOT NULL,
 created_ns INTEGER NOT NULL, durability INTEGER NOT NULL,
 archived_revision BLOB NOT NULL CHECK(length(archived_revision)=8 AND archived_revision<>zeroblob(8)),
 reclaimed_checkpoints INTEGER NOT NULL CHECK(reclaimed_checkpoints>=0),
 receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32));
CREATE TABLE branch_horizons (
 session_id TEXT NOT NULL, branch_id TEXT NOT NULL,
 horizon_revision BLOB NOT NULL CHECK(length(horizon_revision)=8),
 horizon_checkpoint TEXT NOT NULL,
 pruned_checkpoints INTEGER NOT NULL CHECK(pruned_checkpoints>0),
 integrity BLOB NOT NULL CHECK(length(integrity)=32),
 PRIMARY KEY(session_id,branch_id),
 FOREIGN KEY(session_id,branch_id) REFERENCES branches(session_id,branch_id),
 FOREIGN KEY(horizon_checkpoint) REFERENCES checkpoints(checkpoint_id)) WITHOUT ROWID;
CREATE TABLE pruned_checkpoints (
 checkpoint_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, branch_id TEXT NOT NULL,
 FOREIGN KEY(session_id,branch_id) REFERENCES branches(session_id,branch_id)) WITHOUT ROWID;
CREATE TABLE session_feed (
 session_id TEXT NOT NULL, seq INTEGER NOT NULL CHECK(seq>=1), branch_id TEXT NOT NULL,
 change INTEGER NOT NULL CHECK(change BETWEEN 1 AND 6), checkpoint_id TEXT,
 record BLOB NOT NULL, integrity BLOB NOT NULL CHECK(length(integrity)=32),
 CHECK((change IN(1,2,3))=(checkpoint_id IS NOT NULL)),
 PRIMARY KEY(session_id,seq), FOREIGN KEY(session_id) REFERENCES sessions(session_id)) WITHOUT ROWID;
CREATE INDEX checkpoints_branch_revision ON checkpoints(session_id,branch_id,revision);
CREATE INDEX checkpoints_root ON checkpoints(root_kind,root_codec,root_digest);
CREATE INDEX checkpoints_parent ON checkpoints(parent_id);
CREATE INDEX checkpoints_fork ON checkpoints(fork_checkpoint);
CREATE INDEX runtime_object_edges_child ON runtime_object_edges(child_kind,child_codec,child_digest);
CREATE INDEX commit_receipts_expected ON commit_receipts(expected_checkpoint);
CREATE INDEX commit_receipts_session ON commit_receipts(session_id);
CREATE INDEX branches_checkpoint ON branches(checkpoint_id);
CREATE INDEX branches_fork ON branches(fork_checkpoint);
CREATE INDEX create_receipts_checkpoint ON create_receipts(checkpoint_id);
CREATE INDEX fork_receipts_source ON fork_receipts(source_checkpoint);
CREATE INDEX fork_receipts_target ON fork_receipts(target_checkpoint);
CREATE INDEX fork_receipts_source_session ON fork_receipts(source_session);
CREATE INDEX branch_horizons_checkpoint ON branch_horizons(horizon_checkpoint);
CREATE INDEX pruned_checkpoints_branch ON pruned_checkpoints(session_id,branch_id);
";

/// Admits shape, schema, domain and counter integrity. Retained state over
/// the configured bounds is not a refusal: an over-limit store still opens so
/// settlement, recovery and deletion can run; its writes are held to their
/// admission class instead.
fn admit_open(c: &Connection) -> Result<(), SessionStoreError> {
    verify_shape_pragmas(c)?;
    verify_exact_schema(c)?;
    bound_domain(c)?;
    validate_counts(c)
}

fn verify_database(c: &Connection) -> Result<(), SessionStoreError> {
    admit_open(c)?;
    let integrity: String = c
        .query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))
        .map_err(map_sqlite)?;
    if integrity != "ok" {
        return Err(SessionStoreError::corrupt());
    }
    if c.query_row("SELECT 1 FROM pragma_foreign_key_check LIMIT 1", [], |_| {
        Ok(())
    })
    .optional()
    .map_err(map_sqlite)?
    .is_some()
    {
        return Err(SessionStoreError::corrupt());
    }
    load_verified_objects(c)?;
    let orphan: bool = c.query_row("WITH RECURSIVE owned(kind,codec,digest) AS (
        SELECT root_kind,root_codec,root_digest FROM checkpoints
        UNION SELECT kind,codec,digest FROM gc_candidates
        UNION SELECT e.child_kind,e.child_codec,e.child_digest FROM runtime_object_edges e
        JOIN owned o ON e.parent_kind=o.kind AND e.parent_codec=o.codec AND e.parent_digest=o.digest)
        SELECT EXISTS(SELECT 1 FROM runtime_objects r WHERE NOT EXISTS (
        SELECT 1 FROM owned o WHERE o.kind=r.kind AND o.codec=r.codec AND o.digest=r.digest))", [], |r| r.get(0)).map_err(map_sqlite)?;
    if orphan {
        return Err(SessionStoreError::corrupt());
    }
    verify_authority(c)
}

fn load_verified_objects(c: &Connection) -> Result<(), SessionStoreError> {
    let mut children = load_all_children(c)?;
    let mut s = c
        .prepare("SELECT o.kind,o.codec,o.digest,p.bytes,o.byte_len FROM runtime_objects o LEFT JOIN runtime_object_bytes p USING(id) ORDER BY o.kind,o.codec,o.digest")
        .map_err(map_sqlite)?;
    let mut rows = s.query([]).map_err(map_sqlite)?;
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        let a = decode_address(
            r.get(0).map_err(map_sqlite)?,
            r.get(1).map_err(map_sqlite)?,
            r.get(2).map_err(map_sqlite)?,
        )?;
        let bytes: Vec<u8> = r
            .get::<_, Option<Vec<u8>>>(3)
            .map_err(map_sqlite)?
            .ok_or(SessionStoreError::corrupt())?;
        if nonnegative(r.get(4).map_err(map_sqlite)?)? != u64n(bytes.len()) {
            return Err(SessionStoreError::corrupt());
        }
        RuntimeObject::from_store(a, bytes, children.remove(&a).unwrap_or_default())
            .map_err(bundle_error)?;
    }
    if !children.is_empty() {
        return Err(SessionStoreError::corrupt());
    }
    drop(rows);
    drop(s);
    Ok(())
}

fn verify_shape_pragmas(c: &Connection) -> Result<(), SessionStoreError> {
    let fk: i64 = c
        .pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .map_err(map_sqlite)?;
    let trusted: i64 = c
        .pragma_query_value(None, "trusted_schema", |r| r.get(0))
        .map_err(map_sqlite)?;
    let journal: String = c
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .map_err(map_sqlite)?;
    let sync: i64 = c
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .map_err(map_sqlite)?;
    let vacuum: i64 = c
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .map_err(map_sqlite)?;
    if fk != 1
        || trusted != 0
        || !journal.eq_ignore_ascii_case("wal")
        || sync != 2
        || vacuum != INCREMENTAL_AUTO_VACUUM
    {
        return Err(SessionStoreError::BackendUnavailable);
    }
    Ok(())
}

fn verify_exact_schema(c: &Connection) -> Result<(), SessionStoreError> {
    let reference = Connection::open_in_memory().map_err(map_sqlite)?;
    reference.execute_batch(SCHEMA).map_err(map_sqlite)?;
    if schema_objects(c)? == schema_objects(&reference)? {
        return Ok(());
    }
    // The object table's DDL spells the format epoch, so a store another
    // epoch's build wrote at this schema version differs here first: that is
    // an incompatible build, not damage.
    let epoch = i64::from(plakous_agent::internal::RUNTIME_FORMAT_EPOCH);
    if object_table_epoch(c)?.is_some_and(|written| written != epoch) {
        return Err(SessionStoreError::UnsupportedCodec);
    }
    Err(SessionStoreError::corrupt())
}

/// The format epoch the `runtime_objects` codec CHECK admits, when the table
/// exists and spells one.
fn object_table_epoch(c: &Connection) -> Result<Option<i64>, SessionStoreError> {
    let sql: Option<String> = c
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='runtime_objects'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite)?;
    Ok(sql.and_then(|sql| {
        let (_, rest) = sql.split_once("CHECK(codec=")?;
        rest.split_once(')')?.0.parse().ok()
    }))
}

fn schema_objects(
    c: &Connection,
) -> Result<Vec<(String, String, String, String)>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT type,name,tbl_name,sql FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL
             ORDER BY type,name,tbl_name,sql",
        )
        .map_err(map_sqlite)?;
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)
}

fn verify_authority(c: &Connection) -> Result<(), SessionStoreError> {
    verify_receipt_completeness(c)?;
    let snapshot = AuthoritySnapshot::load(c)?;
    snapshot.verify()?;
    feed::verify(c)
}

struct AuthoritySnapshot {
    checkpoints: BTreeMap<CheckpointId, CheckpointRow>,
    branches: BTreeMap<SessionRef, BranchRow>,
}

impl AuthoritySnapshot {
    fn load(c: &Connection) -> Result<Self, SessionStoreError> {
        let sessions = load_authority_sessions(c)?;
        let checkpoints = load_authority_checkpoint_rows(c)?;
        for checkpoint in checkpoints.values() {
            verify_checkpoint_validation_seal(checkpoint)?;
        }
        let branches = load_authority_branches(c, &checkpoints)?;
        load_authority_create_receipts(c, &sessions, &checkpoints, &branches)?;
        load_authority_commit_receipts(c, &checkpoints)?;
        load_authority_fork_receipts(c, &sessions, &checkpoints)?;
        load_authority_archive_receipts(c, &sessions)?;
        load_authority_delete_receipts(c, &sessions)?;
        load_authority_horizons(c, &checkpoints, &branches)?;
        Ok(Self {
            checkpoints,
            branches,
        })
    }

    fn verify(&self) -> Result<(), SessionStoreError> {
        for (selector, branch) in &self.branches {
            let checkpoint = self
                .checkpoints
                .get(&branch.head.checkpoint().checkpoint_id())
                .ok_or(SessionStoreError::corrupt())?;
            if checkpoint.reference != branch.head.checkpoint()
                || checkpoint.selector != *selector
                || checkpoint.revision != branch.head.revision()
                || checkpoint.lineage != branch.lineage
                || checkpoint.fence > branch.last_fence
                || branch
                    .active
                    .as_ref()
                    .is_some_and(|lease| lease.fence().get() != branch.last_fence)
            {
                return Err(SessionStoreError::corrupt());
            }
        }
        Ok(())
    }
}

fn load_authority_sessions(
    c: &Connection,
) -> Result<BTreeMap<SessionId, SessionHead>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,initial_branch,revision,created_s,created_ns,durability,archived
         FROM sessions ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    let mut sessions = BTreeMap::new();
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let head = SessionHead::from_store(
            session_id,
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?)?),
            timestamp(
                &row.get::<_, Vec<u8>>(3).map_err(map_sqlite)?,
                row.get(4).map_err(map_sqlite)?,
            )?,
            decode_durability(row.get(5).map_err(map_sqlite)?)?,
            boolv(row.get(6).map_err(map_sqlite)?)?,
        );
        if sessions.insert(session_id, head).is_some() {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(sessions)
}

fn load_authority_checkpoint_rows(
    c: &Connection,
) -> Result<BTreeMap<CheckpointId, CheckpointRow>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT checkpoint_id,root_kind,root_codec,root_digest,integrity_version,integrity,
         commit_version,commit_digest,session_id,branch_id,revision,parent_id,fork_session,
         fork_branch,fork_checkpoint,fork_revision,fence,kind,durability,semantic_key,
         base_kind,base_codec,base_digest,validation_seal FROM checkpoints ORDER BY checkpoint_id",
        )
        .map_err(map_sqlite)?;
    let mut records = statement.query([]).map_err(map_sqlite)?;
    let mut rows = BTreeMap::new();
    while let Some(record) = records.next().map_err(map_sqlite)? {
        let checkpoint_id = parse(&record.get::<_, String>(0).map_err(map_sqlite)?)?;
        let root = decode_address(
            record.get(1).map_err(map_sqlite)?,
            record.get(2).map_err(map_sqlite)?,
            record.get(3).map_err(map_sqlite)?,
        )?;
        let base = match (
            record.get::<_, Option<i64>>(20).map_err(map_sqlite)?,
            record.get::<_, Option<i64>>(21).map_err(map_sqlite)?,
            record.get::<_, Option<Vec<u8>>>(22).map_err(map_sqlite)?,
        ) {
            (None, None, None) => None,
            (Some(kind), Some(codec), Some(digest)) => Some(decode_address(kind, codec, digest)?),
            _ => return Err(SessionStoreError::corrupt()),
        };
        let fork_session = record.get::<_, Option<String>>(12).map_err(map_sqlite)?;
        let fork_branch = record.get::<_, Option<String>>(13).map_err(map_sqlite)?;
        let fork_checkpoint = record.get::<_, Option<String>>(14).map_err(map_sqlite)?;
        let fork_revision = record.get::<_, Option<Vec<u8>>>(15).map_err(map_sqlite)?;
        let row = CheckpointRow {
            reference: CheckpointObjectRef::from_store(
                checkpoint_id,
                root,
                decode_digest(
                    DigestDomain::Blob,
                    record.get(4).map_err(map_sqlite)?,
                    record.get(5).map_err(map_sqlite)?,
                )?,
            ),
            base,
            commit: decode_digest(
                DigestDomain::ProviderOperation,
                record.get(6).map_err(map_sqlite)?,
                record.get(7).map_err(map_sqlite)?,
            )?,
            selector: SessionRef::new(
                parse(&record.get::<_, String>(8).map_err(map_sqlite)?)?,
                parse(&record.get::<_, String>(9).map_err(map_sqlite)?)?,
            ),
            revision: BranchRevision::from_store(read_u64(
                &record.get::<_, Vec<u8>>(10).map_err(map_sqlite)?,
            )?),
            parent: record
                .get::<_, Option<String>>(11)
                .map_err(map_sqlite)?
                .as_deref()
                .map(parse)
                .transpose()?,
            lineage: lineage(
                fork_session.as_deref(),
                fork_branch.as_deref(),
                fork_checkpoint.as_deref(),
                fork_revision.as_deref(),
            )?,
            fence: read_u64(&record.get::<_, Vec<u8>>(16).map_err(map_sqlite)?)?,
            kind: decode_checkpoint_kind(record.get(17).map_err(map_sqlite)?)?,
            durability: decode_durability(record.get(18).map_err(map_sqlite)?)?,
            key: record.get(19).map_err(map_sqlite)?,
            validation_seal: record
                .get::<_, Option<Vec<u8>>>(23)
                .map_err(map_sqlite)?
                .unwrap_or_default(),
        };
        if rows.insert(checkpoint_id, row).is_some() {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(rows)
}

fn load_authority_branches(
    c: &Connection,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
) -> Result<BTreeMap<SessionRef, BranchRow>, SessionStoreError> {
    let mut statement = c.prepare(
        "SELECT session_id,branch_id,revision,checkpoint_id,last_fence,fork_session,fork_branch,
         fork_checkpoint,fork_revision,active_owner,active_fence,active_time,active_ns,
         active_duration,previous_time,previous_ns,released_owner,released_fence,released_time,
         released_ns,released_duration FROM branches ORDER BY session_id,branch_id",
    ).map_err(map_sqlite)?;
    let mut records = statement.query([]).map_err(map_sqlite)?;
    let mut branches = BTreeMap::new();
    while let Some(record) = records.next().map_err(map_sqlite)? {
        let selector = SessionRef::new(
            parse(&record.get::<_, String>(0).map_err(map_sqlite)?)?,
            parse(&record.get::<_, String>(1).map_err(map_sqlite)?)?,
        );
        let checkpoint_id = parse(&record.get::<_, String>(3).map_err(map_sqlite)?)?;
        let checkpoint = checkpoints
            .get(&checkpoint_id)
            .ok_or(SessionStoreError::corrupt())?;
        let active_owner = record.get::<_, Option<String>>(9).map_err(map_sqlite)?;
        let active_fence = record.get::<_, Option<Vec<u8>>>(10).map_err(map_sqlite)?;
        let active_seconds = record.get::<_, Option<Vec<u8>>>(11).map_err(map_sqlite)?;
        let active_nanos = record.get::<_, Option<i64>>(12).map_err(map_sqlite)?;
        let active_duration = record.get::<_, Option<Vec<u8>>>(13).map_err(map_sqlite)?;
        let active = lease(
            selector,
            active_owner.as_deref(),
            active_fence.as_deref(),
            active_seconds.as_deref(),
            active_nanos,
            active_duration.as_deref(),
        )?;
        let previous_seconds = record.get::<_, Option<Vec<u8>>>(14).map_err(map_sqlite)?;
        let previous_nanos = record.get::<_, Option<i64>>(15).map_err(map_sqlite)?;
        let previous = match (&previous_seconds, previous_nanos, &active) {
            (None, None, _) => None,
            (Some(seconds), Some(nanos), Some(active)) => Some(BranchLease::from_store(
                selector,
                active.owner(),
                active.fence(),
                lease_time(seconds, nanos)?,
                active.lease_seconds(),
            )),
            _ => return Err(SessionStoreError::corrupt()),
        };
        let released_owner = record.get::<_, Option<String>>(16).map_err(map_sqlite)?;
        let released_fence = record.get::<_, Option<Vec<u8>>>(17).map_err(map_sqlite)?;
        let released_seconds = record.get::<_, Option<Vec<u8>>>(18).map_err(map_sqlite)?;
        let released_nanos = record.get::<_, Option<i64>>(19).map_err(map_sqlite)?;
        let released_duration = record.get::<_, Option<Vec<u8>>>(20).map_err(map_sqlite)?;
        let released = lease(
            selector,
            released_owner.as_deref(),
            released_fence.as_deref(),
            released_seconds.as_deref(),
            released_nanos,
            released_duration.as_deref(),
        )?;
        if active.is_some() && released.is_some() {
            return Err(SessionStoreError::corrupt());
        }
        let fork_session = record.get::<_, Option<String>>(5).map_err(map_sqlite)?;
        let fork_branch = record.get::<_, Option<String>>(6).map_err(map_sqlite)?;
        let fork_checkpoint = record.get::<_, Option<String>>(7).map_err(map_sqlite)?;
        let fork_revision = record.get::<_, Option<Vec<u8>>>(8).map_err(map_sqlite)?;
        let branch = BranchRow {
            head: BranchHead::from_store(
                selector,
                BranchRevision::from_store(read_u64(
                    &record.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
                )?),
                checkpoint.reference,
            ),
            last_fence: read_u64(&record.get::<_, Vec<u8>>(4).map_err(map_sqlite)?)?,
            lineage: lineage(
                fork_session.as_deref(),
                fork_branch.as_deref(),
                fork_checkpoint.as_deref(),
                fork_revision.as_deref(),
            )?,
            active,
            previous,
            released,
        };
        if branches.insert(selector, branch).is_some() {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(branches)
}

fn load_authority_create_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
    branches: &BTreeMap<SessionRef, BranchRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,branch_id,created_s,created_ns,durability,owner,duration,
         checkpoint_id,fence,expires_time,expires_ns,receipt_integrity,feed_seq
         FROM create_receipts ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let selector = SessionRef::new(
            session_id,
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
        );
        let created = timestamp(
            &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
            row.get(3).map_err(map_sqlite)?,
        )?;
        let durability = decode_durability(row.get(4).map_err(map_sqlite)?)?;
        let duration = nz(&row.get::<_, Vec<u8>>(6).map_err(map_sqlite)?)?;
        let checkpoint_id = parse(&row.get::<_, String>(7).map_err(map_sqlite)?)?;
        let checkpoint = checkpoints
            .get(&checkpoint_id)
            .ok_or(SessionStoreError::corrupt())?;
        let lease = BranchLease::from_store(
            selector,
            parse(&row.get::<_, String>(5).map_err(map_sqlite)?)?,
            nz(&row.get::<_, Vec<u8>>(8).map_err(map_sqlite)?)?,
            lease_time(
                &row.get::<_, Vec<u8>>(9).map_err(map_sqlite)?,
                row.get(10).map_err(map_sqlite)?,
            )?,
            duration,
        );
        let persisted_session = sessions
            .get(&session_id)
            .ok_or(SessionStoreError::corrupt())?;
        let persisted_branch = branches
            .get(&selector)
            .ok_or(SessionStoreError::corrupt())?;
        if checkpoint.selector != selector
            || checkpoint.revision != BranchRevision::INITIAL
            || checkpoint.parent.is_some()
            || checkpoint.lineage.is_some()
            || checkpoint.fence != lease.fence().get()
            || checkpoint.kind != RuntimeCheckpointKind::Quiescent
            || checkpoint.durability != durability
            || persisted_session.initial_branch() != selector.branch_id()
            || persisted_session.created_at() != created
            || persisted_session.durability() != durability
            || persisted_branch.lineage.is_some()
        {
            return Err(SessionStoreError::corrupt());
        }
        let receipt = CreateReceipt::from_store(
            CreateOperation::from_store(
                selector,
                created,
                lease.owner(),
                duration,
                checkpoint.reference,
                checkpoint.commit,
            ),
            SessionHead::from_store(
                session_id,
                selector.branch_id(),
                SessionRevision::INITIAL,
                created,
                durability,
                false,
            ),
            BranchHead::from_store(selector, BranchRevision::INITIAL, checkpoint.reference),
            lease,
            feed::receipt_seq(row.get(12).map_err(map_sqlite)?)?,
        );
        if create_receipt_integrity(&receipt)? != row.get::<_, Vec<u8>>(11).map_err(map_sqlite)? {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_commit_receipts(
    c: &Connection,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT checkpoint_id,session_id,branch_id,owner,fence,expires_time,expires_ns,duration,
         expected_revision,expected_checkpoint,head_revision,receipt_integrity,feed_seq
         FROM commit_receipts ORDER BY checkpoint_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let checkpoint_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let selector = SessionRef::new(
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(2).map_err(map_sqlite)?)?,
        );
        let lease = BranchLease::from_store(
            selector,
            parse(&row.get::<_, String>(3).map_err(map_sqlite)?)?,
            nz(&row.get::<_, Vec<u8>>(4).map_err(map_sqlite)?)?,
            lease_time(
                &row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?,
                row.get(6).map_err(map_sqlite)?,
            )?,
            nz(&row.get::<_, Vec<u8>>(7).map_err(map_sqlite)?)?,
        );
        let previous_revision =
            BranchRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(8).map_err(map_sqlite)?)?);
        let previous_id = parse(&row.get::<_, String>(9).map_err(map_sqlite)?)?;
        let head_revision =
            BranchRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(10).map_err(map_sqlite)?)?);
        let old = checkpoints
            .get(&previous_id)
            .ok_or(SessionStoreError::corrupt())?;
        let new = checkpoints
            .get(&checkpoint_id)
            .ok_or(SessionStoreError::corrupt())?;
        if previous_revision.checked_next() != Some(head_revision)
            || old.selector != selector
            || new.selector != selector
            || old.revision != previous_revision
            || new.revision != head_revision
            || new.parent != Some(old.reference.checkpoint_id())
            || new.lineage != old.lineage
            || new.fence != lease.fence().get()
            || new.durability != old.durability
        {
            return Err(SessionStoreError::corrupt());
        }
        let previous = BranchHead::from_store(selector, previous_revision, old.reference);
        let head = BranchHead::from_store(selector, head_revision, new.reference);
        let committed = CommittedCheckpoint::from_store(
            CommitOperation::from_store(lease, previous, new.reference, new.commit),
            CommitReceipt::from_store(
                new.reference,
                previous,
                head,
                feed::receipt_seq(row.get(12).map_err(map_sqlite)?)?,
            ),
        );
        if commit_receipt_integrity(&committed)? != row.get::<_, Vec<u8>>(11).map_err(map_sqlite)? {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "ordered bulk receipt decoding keeps exact fork authority visible"
)]
fn load_authority_fork_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT workflow_id,target_session,target_branch,source_session,source_branch,
         expected_session_revision,source_revision,semantic_request_version,semantic_request,
         source_checkpoint,transition,target_checkpoint,target_owner,target_duration,target_fence,
         target_expires_time,target_expires_ns,initial_branch,created_s,created_ns,durability,
         previous_session_revision,session_revision,receipt_integrity,feed_seq
         FROM fork_receipts ORDER BY workflow_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let workflow_id = decode_workflow_id(&row.get::<_, Vec<u8>>(0).map_err(map_sqlite)?)?;
        let target = SessionRef::new(
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(2).map_err(map_sqlite)?)?,
        );
        let source_ref = SessionRef::new(
            parse(&row.get::<_, String>(3).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(4).map_err(map_sqlite)?)?,
        );
        let expected_session_revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?)?);
        let source_revision =
            BranchRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(6).map_err(map_sqlite)?)?);
        let semantic_request = decode_digest(
            DigestDomain::RequestPlan,
            row.get(7).map_err(map_sqlite)?,
            row.get(8).map_err(map_sqlite)?,
        )?;
        let source_id = parse(&row.get::<_, String>(9).map_err(map_sqlite)?)?;
        let target_id = parse(&row.get::<_, String>(11).map_err(map_sqlite)?)?;
        let source_checkpoint = checkpoints
            .get(&source_id)
            .ok_or(SessionStoreError::corrupt())?;
        let target_checkpoint = checkpoints
            .get(&target_id)
            .ok_or(SessionStoreError::corrupt())?;
        let duration = nz(&row.get::<_, Vec<u8>>(13).map_err(map_sqlite)?)?;
        let lease = BranchLease::from_store(
            target,
            parse(&row.get::<_, String>(12).map_err(map_sqlite)?)?,
            nz(&row.get::<_, Vec<u8>>(14).map_err(map_sqlite)?)?,
            lease_time(
                &row.get::<_, Vec<u8>>(15).map_err(map_sqlite)?,
                row.get(16).map_err(map_sqlite)?,
            )?,
            duration,
        );
        let initial = parse(&row.get::<_, String>(17).map_err(map_sqlite)?)?;
        let created = timestamp(
            &row.get::<_, Vec<u8>>(18).map_err(map_sqlite)?,
            row.get(19).map_err(map_sqlite)?,
        )?;
        let durability = decode_durability(row.get(20).map_err(map_sqlite)?)?;
        let previous_revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(21).map_err(map_sqlite)?)?);
        let session_revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(22).map_err(map_sqlite)?)?);
        let source =
            BranchHead::from_store(source_ref, source_revision, source_checkpoint.reference);
        let receipt = ForkReceipt::from_store(
            ForkOperation::from_store(
                workflow_id,
                semantic_request,
                expected_session_revision,
                source,
                target,
                decode_transition(row.get(10).map_err(map_sqlite)?)?,
                target_checkpoint.reference,
                target_checkpoint.commit,
            ),
            SessionHead::from_store(
                target.session_id(),
                initial,
                previous_revision,
                created,
                durability,
                false,
            ),
            SessionHead::from_store(
                target.session_id(),
                initial,
                session_revision,
                created,
                durability,
                false,
            ),
            source,
            BranchHead::from_store(target, BranchRevision::INITIAL, target_checkpoint.reference),
            lease.clone(),
            feed::receipt_seq(row.get(24).map_err(map_sqlite)?)?,
        );
        let persisted = sessions
            .get(&target.session_id())
            .ok_or(SessionStoreError::corrupt())?;
        if persisted.initial_branch() != initial
            || persisted.created_at() != created
            || persisted.durability() != durability
            || source_checkpoint.selector != source_ref
            || source_checkpoint.revision != source_revision
            || target_checkpoint.selector != target
            || target_checkpoint.revision != BranchRevision::INITIAL
            || target_checkpoint.parent != Some(source_id)
            || target_checkpoint.base != Some(source_checkpoint.reference.root())
            || target_checkpoint.lineage
                != Some(ForkLineage {
                    session_id: source_ref.session_id(),
                    branch_id: source_ref.branch_id(),
                    checkpoint_id: source_id,
                    branch_revision: source_revision.get(),
                })
            || target_checkpoint.fence != lease.fence().get()
            || target_checkpoint.kind != RuntimeCheckpointKind::Quiescent
            || target_checkpoint.durability != source_checkpoint.durability
            || fork_receipt_integrity(&receipt)? != row.get::<_, Vec<u8>>(23).map_err(map_sqlite)?
        {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_archive_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,initial_branch,created_s,created_ns,durability,expected_revision,
         session_revision,receipt_integrity FROM archive_receipts ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let initial = parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?;
        let created = timestamp(
            &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
            row.get(3).map_err(map_sqlite)?,
        )?;
        let durability = decode_durability(row.get(4).map_err(map_sqlite)?)?;
        let expected =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?)?);
        let next =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(6).map_err(map_sqlite)?)?);
        let receipt = ArchiveReceipt::from_store(
            ArchiveSession::new(session_id, expected),
            SessionHead::from_store(session_id, initial, expected, created, durability, false),
            SessionHead::from_store(session_id, initial, next, created, durability, true),
        );
        if expected.checked_next() != Some(next)
            || sessions.get(&session_id).copied() != Some(receipt.session())
            || archive_receipt_integrity(receipt)?
                != row.get::<_, Vec<u8>>(7).map_err(map_sqlite)?
        {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_delete_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,initial_branch,created_s,created_ns,durability,archived_revision,
         reclaimed_checkpoints,receipt_integrity
         FROM delete_tombstones ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?)?);
        if revision == SessionRevision::INITIAL || sessions.contains_key(&session_id) {
            return Err(SessionStoreError::corrupt());
        }
        let request = DeleteSession::new(session_id, revision);
        let receipt = DeleteReceipt::from_store(
            request,
            SessionHead::from_store(
                session_id,
                parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
                revision,
                timestamp(
                    &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
                    row.get(3).map_err(map_sqlite)?,
                )?,
                decode_durability(row.get(4).map_err(map_sqlite)?)?,
                true,
            ),
            nonnegative(row.get(6).map_err(map_sqlite)?)?,
        );
        if delete_receipt_integrity(receipt)? != row.get::<_, Vec<u8>>(7).map_err(map_sqlite)? {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_horizons(
    c: &Connection,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
    branches: &BTreeMap<SessionRef, BranchRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,branch_id,horizon_revision,horizon_checkpoint,pruned_checkpoints,integrity FROM branch_horizons
             ORDER BY session_id,branch_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let selector = SessionRef::new(
            parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
        );
        let horizon = decode_horizon(
            selector,
            &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
            &row.get::<_, String>(3).map_err(map_sqlite)?,
            row.get(4).map_err(map_sqlite)?,
            &row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?,
        )?;
        let checkpoint = checkpoints
            .get(&horizon.checkpoint)
            .ok_or(SessionStoreError::corrupt())?;
        let branch = branches
            .get(&selector)
            .ok_or(SessionStoreError::corrupt())?;
        if checkpoint.selector != selector
            || checkpoint.revision != horizon.revision
            || horizon.revision >= branch.head.revision()
        {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn verify_receipt_completeness(c: &Connection) -> Result<(), SessionStoreError> {
    let gap = c
        .query_row(
            ("SELECT 1 WHERE
             EXISTS(SELECT 1 FROM sessions s LEFT JOIN create_receipts r USING(session_id)
                    WHERE r.session_id IS NULL)
             OR EXISTS(SELECT 1 FROM sessions s LEFT JOIN archive_receipts r USING(session_id)
                    WHERE (s.archived=1 AND r.session_id IS NULL)
                       OR (s.archived=0 AND r.session_id IS NOT NULL))
             OR EXISTS(SELECT 1 FROM branches b LEFT JOIN fork_receipts r
                    ON r.target_session=b.session_id AND r.target_branch=b.branch_id
                    WHERE (b.fork_checkpoint IS NOT NULL AND r.target_session IS NULL)
                       OR (b.fork_checkpoint IS NULL AND r.target_session IS NOT NULL))
             OR EXISTS(SELECT 1 FROM checkpoints c WHERE
                    (c.revision=zeroblob(8)
                     AND NOT EXISTS(SELECT 1 FROM create_receipts r
                                    WHERE r.checkpoint_id=c.checkpoint_id)
                     AND NOT EXISTS(SELECT 1 FROM fork_receipts r
                                    WHERE r.target_checkpoint=c.checkpoint_id))
                    OR (c.revision<>zeroblob(8)
                        AND NOT EXISTS(SELECT 1 FROM commit_receipts r
                                       WHERE r.checkpoint_id=c.checkpoint_id)
                        AND NOT EXISTS(SELECT 1 FROM branch_horizons h
                                       WHERE h.session_id=c.session_id
                                         AND h.branch_id=c.branch_id
                                         AND c.revision<=h.horizon_revision)))
             OR EXISTS(SELECT 1 FROM commit_receipts r
                    JOIN checkpoints c ON c.checkpoint_id=r.checkpoint_id
                    JOIN branch_horizons h ON h.session_id=c.session_id AND h.branch_id=c.branch_id
                    WHERE c.revision<=h.horizon_revision)
             OR EXISTS(SELECT 1 FROM pruned_checkpoints p
                    JOIN checkpoints c ON c.checkpoint_id=p.checkpoint_id)
             OR EXISTS(SELECT 1 FROM checkpoints c WHERE c.parent_id IS NOT NULL
                    AND NOT EXISTS(SELECT 1 FROM checkpoints p
                                   WHERE p.checkpoint_id=c.parent_id)
                    AND NOT EXISTS(SELECT 1 FROM pruned_checkpoints p
                                   WHERE p.checkpoint_id=c.parent_id))"
                .to_owned()
                + feed::COMPLETENESS)
                .as_str(),
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(map_sqlite)?;
    if gap.is_some() {
        Err(SessionStoreError::corrupt())
    } else {
        Ok(())
    }
}

fn read_stats(c: &Connection) -> Result<SqliteStoreStats, SessionStoreError> {
    let v = c.query_row(
        "SELECT sessions,branches,checkpoints,objects,object_bytes,semantic_mutations,tombstones,reclaimed_objects,reclaimed_object_bytes FROM store_meta WHERE id=1",
        [], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,
                    r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?))
    ).map_err(map_sqlite)?;
    Ok(SqliteStoreStats {
        sessions: nonnegative(v.0)?,
        branches: nonnegative(v.1)?,
        checkpoints: nonnegative(v.2)?,
        objects: nonnegative(v.3)?,
        object_bytes: nonnegative(v.4)?,
        semantic_mutations: nonnegative(v.5)?,
        tombstones: nonnegative(v.6)?,
        reclaimed_objects: nonnegative(v.7)?,
        reclaimed_object_bytes: nonnegative(v.8)?,
    })
}

fn validate_counts(c: &Connection) -> Result<(), SessionStoreError> {
    let s = read_stats(c)?;
    let count = |table: &str| -> Result<u64, SessionStoreError> {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(map_sqlite)
        .and_then(nonnegative)
    };
    let bytes=c.query_row(
        "SELECT COALESCE(sum(byte_len),0)+(SELECT count(*)*37 FROM runtime_object_edges) FROM runtime_objects",
        [],|r|r.get::<_,i64>(0)).map_err(map_sqlite).and_then(nonnegative)?;
    if (
        s.sessions,
        s.branches,
        s.checkpoints,
        s.objects,
        s.object_bytes,
        s.tombstones,
    ) != (
        count("sessions")?,
        count("branches")?,
        count("checkpoints")?,
        count("runtime_objects")?,
        bytes,
        count("delete_tombstones")?,
    ) {
        return Err(SessionStoreError::corrupt());
    }
    Ok(())
}

struct Candidate {
    reference: CheckpointObjectRef,
    base: Option<RuntimeObjectAddress>,
    checkpoint: Arc<RuntimeCheckpoint>,
    objects: Vec<RuntimeObject>,
    semantic_key: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct InsertEvidence {
    objects: u64,
    bytes: u64,
}

fn candidate(b: &CheckpointObjectBundle) -> Result<Candidate, SessionStoreError> {
    let checkpoint = b.validated_checkpoint().map_err(bundle_error)?;
    let reference = bundle_ref(b);
    if checkpoint.manifest().topology().checkpoint_id != reference.checkpoint_id()
        || checkpoint.integrity() != reference.integrity()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointObjectBinding,
        ));
    }
    let objects = b.supplied_objects().to_vec();
    if objects.windows(2).any(|p| p[0].address() >= p[1].address()) {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::NoncanonicalCheckpointObjects,
        ));
    }
    let semantic_key = semantic_key(checkpoint.manifest(), &objects)?;
    Ok(Candidate {
        reference,
        base: b.base_root(),
        checkpoint,
        objects,
        semantic_key,
    })
}

fn bundle_ref(b: &CheckpointObjectBundle) -> CheckpointObjectRef {
    CheckpointObjectRef::from_store(b.checkpoint_id(), b.root(), b.checkpoint_integrity())
}

fn semantic_key(
    m: &RuntimeManifest,
    objects: &[RuntimeObject],
) -> Result<Vec<u8>, SessionStoreError> {
    let roots = objects
        .iter()
        .filter(|o| o.address().kind() == RuntimeObjectKind::ManifestRoot)
        .collect::<Vec<_>>();
    if roots.len() != 1
        || roots[0].children().first().map(|a| a.kind())
            != Some(RuntimeObjectKind::ManifestMetadata)
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::ManifestRootDelta,
        ));
    }
    let mut out = b"plakous-store/semantic-key-v5".to_vec();
    out.push(durability_tag(m.durability())?);
    for a in roots[0].children().iter().skip(1) {
        encode_address(&mut out, *a);
    }
    Ok(out)
}

fn insert_candidate(
    tx: &Transaction<'_>,
    x: &Candidate,
    limits: MemoryStoreLimits,
    admission: StoreAdmission,
) -> Result<InsertEvidence, SessionStoreError> {
    if checkpoint_row(tx, x.reference.checkpoint_id())?.is_some() {
        return Err(SessionStoreError::IdentityConflict);
    }
    if let Some(base) = x.base
        && !object_exists(tx, base)?
    {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingBase,
        ));
    }
    let supplied = x
        .objects
        .iter()
        .map(RuntimeObject::address)
        .collect::<BTreeSet<_>>();
    if !supplied.contains(&x.reference.root()) && !object_exists(tx, x.reference.root())? {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingRoot,
        ));
    }
    let mut missing = Vec::new();
    let mut added = 0_u64;
    for object in &x.objects {
        for child in object.children() {
            if !supplied.contains(child) && !object_exists(tx, *child)? {
                return Err(SessionStoreError::Corrupt(
                    plakous_agent::internal::StoreIntegrityReason::MissingChild,
                ));
            }
        }
        if object_exists(tx, object.address())? {
            if load_object(tx, object.address())?.as_ref() != Some(object) {
                return Err(SessionStoreError::corrupt());
            }
        } else {
            added = added
                .checked_add(u64n(object.retained_bytes()))
                .ok_or_else(|| {
                    SessionStoreError::capacity(StoreCapacityBound::ObjectBytes, u64::MAX, 0)
                })?;
            missing.push(object);
        }
    }
    let stats = read_stats(tx)?;
    let objects = stats
        .objects
        .checked_add(u64n(missing.len()))
        .ok_or_else(|| SessionStoreError::capacity(StoreCapacityBound::Objects, u64::MAX, 0))?;
    let bytes = stats
        .object_bytes
        .checked_add(added)
        .ok_or_else(|| SessionStoreError::capacity(StoreCapacityBound::ObjectBytes, u64::MAX, 0))?;
    let checkpoints = stats
        .checkpoints
        .checked_add(1)
        .ok_or_else(|| SessionStoreError::capacity(StoreCapacityBound::Checkpoints, u64::MAX, 0))?;
    admit(
        StoreCapacityBound::Objects,
        objects,
        limits.max_objects().get(),
        admission,
    )?;
    admit(
        StoreCapacityBound::ObjectBytes,
        bytes,
        limits.max_object_bytes().get(),
        admission,
    )?;
    admit(
        StoreCapacityBound::Checkpoints,
        checkpoints,
        limits.max_checkpoints().get(),
        admission,
    )?;
    let missing_count = u64n(missing.len());
    for object in &missing {
        insert_object_row(tx, object)?;
    }
    for object in missing {
        insert_object_edges(tx, object)?;
    }
    insert_checkpoint(tx, x)?;
    tx.execute(
        "UPDATE store_meta SET checkpoints=?1,objects=?2,object_bytes=?3 WHERE id=1",
        params![i64n(checkpoints)?, i64n(objects)?, i64n(bytes)?],
    )
    .map_err(map_sqlite)?;
    Ok(InsertEvidence {
        objects: missing_count,
        bytes: added,
    })
}

fn insert_object_row(tx: &Transaction<'_>, o: &RuntimeObject) -> Result<(), SessionStoreError> {
    let a = o.address();
    tx.execute(
        "INSERT INTO runtime_objects(kind,codec,digest,byte_len) VALUES(?1,?2,?3,?4)",
        params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice(),
            i64n(u64n(o.canonical_bytes().len()))?
        ],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "INSERT INTO runtime_object_bytes(id,bytes) VALUES(?1,?2)",
        params![tx.last_insert_rowid(), o.canonical_bytes()],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

fn insert_object_edges(tx: &Transaction<'_>, o: &RuntimeObject) -> Result<(), SessionStoreError> {
    let a = o.address();
    for (p, c) in o.children().iter().enumerate() {
        tx.execute(
            "INSERT INTO runtime_object_edges VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                kind_tag(a.kind()),
                i64::from(a.codec_revision()),
                a.digest().bytes().as_slice(),
                i64n(u64n(p))?,
                kind_tag(c.kind()),
                i64::from(c.codec_revision()),
                c.digest().bytes().as_slice()
            ],
        )
        .map_err(map_sqlite)?;
    }
    Ok(())
}

fn insert_checkpoint(tx: &Transaction<'_>, x: &Candidate) -> Result<(), SessionStoreError> {
    let m = x.checkpoint.manifest();
    let t = m.topology();
    let r = x.reference.root();
    let integrity = x.reference.integrity();
    let commit = x.checkpoint.checkpoint_commit_identity();
    let fork = t.fork;
    let base = x.base;
    let checkpoint_row = CheckpointRow {
        reference: x.reference,
        base,
        commit,
        selector: SessionRef::new(t.session_id, t.branch_id),
        revision: BranchRevision::from_store(t.branch_revision),
        parent: t.parent_checkpoint_id,
        lineage: fork,
        fence: t.lease_fence,
        kind: x.checkpoint.kind(),
        durability: m.durability(),
        key: x.semantic_key.clone(),
        validation_seal: Vec::new(),
    };
    let validation_seal = checkpoint_validation_seal(&checkpoint_row)?;
    tx.execute(
        "INSERT INTO checkpoints VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,
         ?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
        params![
            id(x.reference.checkpoint_id()),
            kind_tag(r.kind()),
            i64::from(r.codec_revision()),
            r.digest().bytes().as_slice(),
            i64::from(integrity.version()),
            integrity.bytes().as_slice(),
            i64::from(commit.version()),
            commit.bytes().as_slice(),
            base.map(|a| kind_tag(a.kind())),
            base.map(|a| i64::from(a.codec_revision())),
            base.map(|a| a.digest().bytes().to_vec()),
            id(t.session_id),
            id(t.branch_id),
            u64b(t.branch_revision),
            t.parent_checkpoint_id.map(id),
            fork.map(|v| id(v.session_id)),
            fork.map(|v| id(v.branch_id)),
            fork.map(|v| id(v.checkpoint_id)),
            fork.map(|v| u64b(v.branch_revision)),
            u64b(t.lease_fence),
            checkpoint_kind_tag(x.checkpoint.kind()),
            durability_tag(m.durability())?,
            &x.semantic_key,
            validation_seal
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

#[derive(Clone)]
struct CheckpointRow {
    reference: CheckpointObjectRef,
    base: Option<RuntimeObjectAddress>,
    commit: Digest,
    selector: SessionRef,
    revision: BranchRevision,
    parent: Option<CheckpointId>,
    lineage: Option<ForkLineage>,
    fence: u64,
    kind: RuntimeCheckpointKind,
    durability: RuntimeDurability,
    key: Vec<u8>,
    validation_seal: Vec<u8>,
}

fn checkpoint_validation_seal(row: &CheckpointRow) -> Result<Vec<u8>, SessionStoreError> {
    let mut selector = text_bytes(row.selector.session_id());
    selector.extend_from_slice(&text_bytes(row.selector.branch_id()));
    let mut base = vec![u8::from(row.base.is_some())];
    if let Some(address) = row.base {
        encode_address(&mut base, address);
    }
    let mut parent = vec![u8::from(row.parent.is_some())];
    if let Some(checkpoint_id) = row.parent {
        parent.extend_from_slice(&text_bytes(checkpoint_id));
    }
    let mut lineage = vec![u8::from(row.lineage.is_some())];
    if let Some(fork) = row.lineage {
        lineage.extend_from_slice(&text_bytes(fork.session_id));
        lineage.extend_from_slice(&text_bytes(fork.branch_id));
        lineage.extend_from_slice(&text_bytes(fork.checkpoint_id));
        lineage.extend_from_slice(&fork.branch_revision.to_be_bytes());
    }
    receipt_integrity(
        b"checkpoint-row-v1",
        &[
            checkpoint_ref_bytes(row.reference),
            base,
            row.commit.identity_bytes(),
            selector,
            row.revision.get().to_be_bytes().to_vec(),
            parent,
            lineage,
            row.fence.to_be_bytes().to_vec(),
            checkpoint_kind_tag(row.kind).to_be_bytes().to_vec(),
            vec![durability_tag(row.durability)?],
            row.key.clone(),
        ],
    )
}

fn verify_checkpoint_validation_seal(row: &CheckpointRow) -> Result<(), SessionStoreError> {
    if checkpoint_validation_seal(row)? != row.validation_seal {
        return Err(SessionStoreError::corrupt());
    }
    Ok(())
}

fn checkpoint_row(
    c: &Connection,
    idv: CheckpointId,
) -> Result<Option<CheckpointRow>, SessionStoreError> {
    c.query_row(
        "SELECT root_kind,root_codec,root_digest,integrity_version,integrity,
        commit_version,commit_digest,session_id,branch_id,revision,parent_id,fork_session,
        fork_branch,fork_checkpoint,fork_revision,fence,kind,durability,semantic_key,
        base_kind,base_codec,base_digest,validation_seal
        FROM checkpoints WHERE checkpoint_id=?1",
        [id(idv)],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Vec<u8>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Vec<u8>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<String>>(13)?,
                r.get::<_, Option<Vec<u8>>>(14)?,
                r.get::<_, Vec<u8>>(15)?,
                r.get::<_, i64>(16)?,
                r.get::<_, i64>(17)?,
                r.get::<_, Vec<u8>>(18)?,
                r.get::<_, Option<i64>>(19)?,
                r.get::<_, Option<i64>>(20)?,
                r.get::<_, Option<Vec<u8>>>(21)?,
                r.get::<_, Option<Vec<u8>>>(22)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let root = decode_address(v.0, v.1, v.2)?;
        let base = match (v.19, v.20, v.21) {
            (None, None, None) => None,
            (Some(kind), Some(codec), Some(digest)) => Some(decode_address(kind, codec, digest)?),
            _ => return Err(SessionStoreError::corrupt()),
        };
        let row = CheckpointRow {
            reference: CheckpointObjectRef::from_store(
                idv,
                root,
                decode_digest(DigestDomain::Blob, v.3, v.4)?,
            ),
            base,
            commit: decode_digest(DigestDomain::ProviderOperation, v.5, v.6)?,
            selector: SessionRef::new(parse(&v.7)?, parse(&v.8)?),
            revision: BranchRevision::from_store(read_u64(&v.9)?),
            parent: v.10.as_deref().map(parse).transpose()?,
            lineage: lineage(
                v.11.as_deref(),
                v.12.as_deref(),
                v.13.as_deref(),
                v.14.as_deref(),
            )?,
            fence: read_u64(&v.15)?,
            kind: decode_checkpoint_kind(v.16)?,
            durability: decode_durability(v.17)?,
            key: v.18,
            validation_seal: v.22.unwrap_or_default(),
        };
        verify_checkpoint_validation_seal(&row)?;
        Ok(row)
    })
    .transpose()
}

fn load_object(
    c: &Connection,
    a: RuntimeObjectAddress,
) -> Result<Option<RuntimeObject>, SessionStoreError> {
    c.query_row(
        "SELECT p.bytes FROM runtime_objects o JOIN runtime_object_bytes p USING(id) WHERE kind=?1 AND codec=?2 AND digest=?3",
        params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice()
        ],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|b| RuntimeObject::from_store(a, b, load_children(c, a)?).map_err(bundle_error))
    .transpose()
}
fn object_exists(c: &Connection, a: RuntimeObjectAddress) -> Result<bool, SessionStoreError> {
    Ok(c.query_row(
        "SELECT 1 FROM runtime_objects WHERE kind=?1 AND codec=?2 AND digest=?3",
        params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice()
        ],
        |_| Ok(()),
    )
    .optional()
    .map_err(map_sqlite)?
    .is_some())
}
fn load_children(
    c: &Connection,
    a: RuntimeObjectAddress,
) -> Result<Vec<RuntimeObjectAddress>, SessionStoreError> {
    let mut s = c
        .prepare(
            "SELECT position,child_kind,child_codec,child_digest FROM runtime_object_edges
        WHERE parent_kind=?1 AND parent_codec=?2 AND parent_digest=?3 ORDER BY position",
        )
        .map_err(map_sqlite)?;
    let mut rows = s
        .query(params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice()
        ])
        .map_err(map_sqlite)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        if r.get::<_, i64>(0).map_err(map_sqlite)? != i64n(u64n(out.len()))? {
            return Err(SessionStoreError::corrupt());
        }
        out.push(decode_address(
            r.get(1).map_err(map_sqlite)?,
            r.get(2).map_err(map_sqlite)?,
            r.get(3).map_err(map_sqlite)?,
        )?);
    }
    Ok(out)
}

fn load_all_children(
    c: &Connection,
) -> Result<BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT parent_kind,parent_codec,parent_digest,position,
             child_kind,child_codec,child_digest FROM runtime_object_edges
             ORDER BY parent_kind,parent_codec,parent_digest,position",
        )
        .map_err(map_sqlite)?;
    let rows = statement.query([]).map_err(map_sqlite)?;
    collect_children(rows)
}

fn collect_children(
    mut rows: rusqlite::Rows<'_>,
) -> Result<BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>>, SessionStoreError> {
    let mut children: BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>> = BTreeMap::new();
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let parent = decode_address(
            row.get(0).map_err(map_sqlite)?,
            row.get(1).map_err(map_sqlite)?,
            row.get(2).map_err(map_sqlite)?,
        )?;
        let values = children.entry(parent).or_default();
        if row.get::<_, i64>(3).map_err(map_sqlite)? != i64n(u64n(values.len()))? {
            return Err(SessionStoreError::corrupt());
        }
        values.push(decode_address(
            row.get(4).map_err(map_sqlite)?,
            row.get(5).map_err(map_sqlite)?,
            row.get(6).map_err(map_sqlite)?,
        )?);
    }
    Ok(children)
}

fn load_reachable_children(
    c: &Connection,
    root: RuntimeObjectAddress,
) -> Result<BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "WITH RECURSIVE reach(kind,codec,digest) AS(
             VALUES(?1,?2,?3) UNION SELECT e.child_kind,e.child_codec,e.child_digest
             FROM runtime_object_edges e JOIN reach p ON e.parent_kind=p.kind
             AND e.parent_codec=p.codec AND e.parent_digest=p.digest)
             SELECT e.parent_kind,e.parent_codec,e.parent_digest,e.position,
             e.child_kind,e.child_codec,e.child_digest FROM reach
             JOIN runtime_object_edges e ON e.parent_kind=reach.kind
             AND e.parent_codec=reach.codec AND e.parent_digest=reach.digest
             ORDER BY e.parent_kind,e.parent_codec,e.parent_digest,e.position",
        )
        .map_err(map_sqlite)?;
    let rows = statement
        .query(params![
            kind_tag(root.kind()),
            i64::from(root.codec_revision()),
            root.digest().bytes().as_slice()
        ])
        .map_err(map_sqlite)?;
    collect_children(rows)
}

#[allow(
    clippy::too_many_lines,
    reason = "the single create transaction stays linear so validation and publication are auditable"
)]
fn create(b: &mut Backend, r: &CreateSession) -> Result<CreateReceipt, SessionStoreError> {
    b.require_open()?;
    let reference = bundle_ref(r.initial());
    let x = candidate(r.initial())?;
    validate_genesis(&x, r)?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let expires = expiry(sample_clock(b.clock.as_ref(), b.domain)?, r.lease_seconds())?;
    if let Some(old) = load_create(&tx, r.session().session_id())? {
        let result = if create_matches(
            &old,
            r,
            reference,
            x.checkpoint.checkpoint_commit_identity(),
        ) {
            Ok(old)
        } else {
            Err(SessionStoreError::IdentityConflict)
        };
        tx.commit().map_err(map_sqlite)?;
        return result;
    }
    require_unused_session_identity(&tx, r.session().session_id())?;
    let stats = read_stats(&tx)?;
    enforce(
        StoreCapacityBound::Sessions,
        stats.sessions + 1,
        b.limits.max_sessions().get(),
    )?;
    insert_candidate(&tx, &x, b.limits, r.admission())?;
    let d = x.checkpoint.manifest().durability();
    let lease_fence = NonZeroU64::MIN;
    let lease = BranchLease::from_store(
        r.session(),
        r.owner(),
        lease_fence,
        expires,
        r.lease_seconds(),
    );
    let head = BranchHead::from_store(r.session(), BranchRevision::INITIAL, x.reference);
    let session = SessionHead::from_store(
        r.session().session_id(),
        r.session().branch_id(),
        SessionRevision::INITIAL,
        r.created_at(),
        d,
        false,
    );
    tx.execute(
        "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,0,0,1,0)",
        params![
            id(session.session_id()),
            id(session.initial_branch()),
            u64b(0),
            u64b(session.created_at().seconds()),
            i64::from(session.created_at().nanoseconds()),
            durability_tag(d)?
        ],
    )
    .map_err(map_sqlite)?;
    insert_branch(&tx, head, None, 1, Some(&lease))?;
    let feed_seq = feed::append(
        &tx,
        r.session(),
        FeedChange::Created {
            genesis: x.reference.checkpoint_id(),
            revision: BranchRevision::INITIAL,
        },
        r.feed_facts(),
    )?;
    let receipt = CreateReceipt::from_store(
        CreateOperation::from_store(
            r.session(),
            r.created_at(),
            r.owner(),
            r.lease_seconds(),
            reference,
            x.checkpoint.checkpoint_commit_identity(),
        ),
        session,
        head,
        lease,
        feed_seq,
    );
    let receipt_integrity = create_receipt_integrity(&receipt)?;
    exactly_one(
        tx.execute(
            "INSERT INTO create_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                id(r.session().session_id()),
                id(r.session().branch_id()),
                u64b(r.created_at().seconds()),
                i64::from(r.created_at().nanoseconds()),
                durability_tag(d)?,
                id(r.owner()),
                u64b(r.lease_seconds().get()),
                id(reference.checkpoint_id()),
                u64b(1),
                lease_time_bytes(expires),
                i64::from(expires.nanoseconds()),
                receipt_integrity,
                i64n(feed_seq.get())?
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    meta(&tx, 1, 1)?;
    commit_claimed(tx, &mut b.holders, r.session(), lease_fence)?;
    Ok(receipt)
}

fn validate_genesis(x: &Candidate, r: &CreateSession) -> Result<(), SessionStoreError> {
    let m = x.checkpoint.manifest();
    let t = m.topology();
    if x.checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || x.base.is_some()
        || t.session_id != r.session().session_id()
        || t.branch_id != r.session().branch_id()
        || t.checkpoint_id != x.reference.checkpoint_id()
        || t.parent_checkpoint_id.is_some()
        || t.branch_revision != 0
        || t.fork.is_some()
        || t.lease_fence != 1
        || matches!(m.durability(), RuntimeDurability::Disabled)
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::GenesisCheckpointTopology,
        ));
    }
    Ok(())
}
fn recover_create(c: &Connection, r: &CreateSession) -> Result<CreateReceipt, SessionStoreError> {
    let reference = bundle_ref(r.initial());
    let checkpoint_commit_identity = r
        .initial()
        .validated_checkpoint()
        .map_err(bundle_error)?
        .checkpoint_commit_identity();
    match load_create(c, r.session().session_id())? {
        Some(v) if create_matches(&v, r, reference, checkpoint_commit_identity) => Ok(v),
        Some(_) => Err(SessionStoreError::IdentityConflict),
        None => Err(SessionStoreError::NotFound),
    }
}
fn load_create(c: &Connection, sid: SessionId) -> Result<Option<CreateReceipt>, SessionStoreError> {
    c.query_row(
        "SELECT branch_id,created_s,created_ns,durability,owner,duration,checkpoint_id,
        fence,expires_time,expires_ns,receipt_integrity,feed_seq FROM create_receipts WHERE session_id=?1",
        [id(sid)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Vec<u8>>(7)?,
                r.get::<_, Vec<u8>>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, Vec<u8>>(10)?,
                r.get::<_, i64>(11)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let selector = SessionRef::new(sid, parse(&v.0)?);
        let created = timestamp(&v.1, v.2)?;
        let d = decode_durability(v.3)?;
        let owner = parse(&v.4)?;
        let duration = nz(&v.5)?;
        let row = checkpoint_row(c, parse(&v.6)?)?.ok_or(SessionStoreError::corrupt())?;
        let lease =
            BranchLease::from_store(selector, owner, nz(&v.7)?, lease_time(&v.8, v.9)?, duration);
        let persisted_session = session_head(c, sid)?.ok_or(SessionStoreError::corrupt())?;
        let persisted_branch = branch_row(c, selector)?.ok_or(SessionStoreError::corrupt())?;
        if row.selector != selector
            || row.revision != BranchRevision::INITIAL
            || row.parent.is_some()
            || row.lineage.is_some()
            || row.fence != lease.fence().get()
            || row.kind != RuntimeCheckpointKind::Quiescent
            || row.durability != d
            || persisted_session.initial_branch() != selector.branch_id()
            || persisted_session.created_at() != created
            || persisted_session.durability() != d
            || persisted_branch.lineage.is_some()
        {
            return Err(SessionStoreError::corrupt());
        }
        let session = SessionHead::from_store(
            sid,
            selector.branch_id(),
            SessionRevision::INITIAL,
            created,
            d,
            false,
        );
        let head = BranchHead::from_store(selector, BranchRevision::INITIAL, row.reference);
        let receipt = CreateReceipt::from_store(
            CreateOperation::from_store(
                selector,
                created,
                owner,
                duration,
                row.reference,
                row.commit,
            ),
            session,
            head,
            lease.clone(),
            feed::receipt_seq(v.11)?,
        );
        if create_receipt_integrity(&receipt)? != v.10 {
            return Err(SessionStoreError::corrupt());
        }
        Ok(receipt)
    })
    .transpose()
}
fn create_matches(
    v: &CreateReceipt,
    r: &CreateSession,
    x: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
) -> bool {
    let o = v.operation();
    o.session() == r.session()
        && o.created_at() == r.created_at()
        && o.owner() == r.owner()
        && o.lease_seconds() == r.lease_seconds()
        && o.initial() == x
        && o.checkpoint_commit_identity() == checkpoint_commit_identity
}

#[derive(Clone)]
struct BranchRow {
    head: BranchHead,
    last_fence: u64,
    lineage: Option<ForkLineage>,
    active: Option<BranchLease>,
    previous: Option<BranchLease>,
    released: Option<BranchLease>,
}
fn branch_row(c: &Connection, s: SessionRef) -> Result<Option<BranchRow>, SessionStoreError> {
    branch_row_with_checkpoint(c, s).map(|row| row.map(|(branch, _)| branch))
}

fn branch_row_with_checkpoint(
    c: &Connection,
    s: SessionRef,
) -> Result<Option<(BranchRow, CheckpointRow)>, SessionStoreError> {
    c.query_row("SELECT revision,checkpoint_id,last_fence,fork_session,fork_branch,fork_checkpoint,
        fork_revision,active_owner,active_fence,active_time,active_ns,active_duration,
        previous_time,previous_ns,released_owner,released_fence,released_time,released_ns,released_duration
        FROM branches WHERE session_id=?1 AND branch_id=?2",params![id(s.session_id()),id(s.branch_id())],|r|Ok((
        r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,
        r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,
        r.get::<_,Option<Vec<u8>>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,Option<Vec<u8>>>(8)?,
        r.get::<_,Option<Vec<u8>>>(9)?,r.get::<_,Option<i64>>(10)?,r.get::<_,Option<Vec<u8>>>(11)?,
        r.get::<_,Option<Vec<u8>>>(12)?,r.get::<_,Option<i64>>(13)?,r.get::<_,Option<String>>(14)?,
        r.get::<_,Option<Vec<u8>>>(15)?,r.get::<_,Option<Vec<u8>>>(16)?,
        r.get::<_,Option<i64>>(17)?,r.get::<_,Option<Vec<u8>>>(18)?)))
    .optional().map_err(map_sqlite)?.map(|v|{
        let cp=checkpoint_row(c,parse(&v.1)?)?.ok_or(SessionStoreError::corrupt())?;
        let active=lease(s,v.7.as_deref(),v.8.as_deref(),v.9.as_deref(),v.10,v.11.as_deref())?;
        let previous=match(&v.12,v.13,&active){(None,None,_)=>None,(Some(sec),Some(ns),Some(a))=>
            Some(BranchLease::from_store(s,a.owner(),a.fence(),lease_time(sec,ns)?,a.lease_seconds())),
            _=>return Err(SessionStoreError::corrupt())};
        let released=lease(s,v.14.as_deref(),v.15.as_deref(),v.16.as_deref(),v.17,v.18.as_deref())?;
        if active.is_some()&&released.is_some(){return Err(SessionStoreError::corrupt());}
        Ok((BranchRow{head:BranchHead::from_store(s,BranchRevision::from_store(read_u64(&v.0)?),cp.reference),
            last_fence:read_u64(&v.2)?,lineage:lineage(v.3.as_deref(),v.4.as_deref(),v.5.as_deref(),v.6.as_deref())?,
            active,previous,released}, cp))
    }).transpose()
}
fn insert_branch(
    tx: &Transaction<'_>,
    h: BranchHead,
    source: Option<BranchHead>,
    fence: u64,
    active: Option<&BranchLease>,
) -> Result<(), SessionStoreError> {
    let l = source.map(|x| ForkLineage {
        session_id: x.session().session_id(),
        branch_id: x.session().branch_id(),
        checkpoint_id: x.checkpoint().checkpoint_id(),
        branch_revision: x.revision().get(),
    });
    tx.execute(
        "INSERT INTO branches(session_id,branch_id,revision,checkpoint_id,last_fence,
        fork_session,fork_branch,fork_checkpoint,fork_revision,active_owner,active_fence,
        active_time,active_ns,active_duration) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            id(h.session().session_id()),
            id(h.session().branch_id()),
            u64b(h.revision().get()),
            id(h.checkpoint().checkpoint_id()),
            u64b(fence),
            l.map(|x| id(x.session_id)),
            l.map(|x| id(x.branch_id)),
            l.map(|x| id(x.checkpoint_id)),
            l.map(|x| u64b(x.branch_revision)),
            active.map(|x| id(x.owner())),
            active.map(|x| u64b(x.fence().get())),
            active.map(|x| lease_time_bytes(x.expires_at())),
            active.map(|x| i64::from(x.expires_at().nanoseconds())),
            active.map(|x| u64b(x.lease_seconds().get()))
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}
fn session_head(c: &Connection, sid: SessionId) -> Result<Option<SessionHead>, SessionStoreError> {
    c.query_row("SELECT initial_branch,revision,created_s,created_ns,durability,archived FROM sessions WHERE session_id=?1",
        [id(sid)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,Vec<u8>>(2)?,
        r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?))).optional().map_err(map_sqlite)?
        .map(|v|Ok(SessionHead::from_store(sid,parse(&v.0)?,SessionRevision::from_store(read_u64(&v.1)?),
            timestamp(&v.2,v.3)?,decode_durability(v.4)?,boolv(v.5)?))).transpose()
}
fn live_session(c: &Connection, s: SessionId) -> Result<SessionHead, SessionStoreError> {
    let h = session_head(c, s)?.ok_or(SessionStoreError::NotFound)?;
    if h.is_archived() {
        Err(SessionStoreError::Archived)
    } else {
        Ok(h)
    }
}

fn acquire(b: &mut Backend, r: AcquireBranch) -> Result<BranchLease, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, r.session().session_id())?;
    let row = branch_row(&tx, r.session())?.ok_or(SessionStoreError::NotFound)?;
    if let Some(current) = row.active.as_ref()
        && !expired(current, now)?
    {
        if current.owner() == r.owner() && current.lease_seconds() == r.lease_seconds() {
            let out = current.clone();
            tx.commit().map_err(map_sqlite)?;
            return Ok(out);
        }
        // A lease exists to stop two live processes sharing a branch. When the
        // holder's process is provably gone, waiting out its clock protects
        // nothing and reads to the next user as a session that will not open.
        // Anything short of proof keeps the clock. Probed inside the writer's
        // transaction, which serializes it against every other reclaim.
        if !b.holders.holder_is_gone(r.session(), current.fence()) {
            tx.commit().map_err(map_sqlite)?;
            return Err(SessionStoreError::LeaseHeld);
        }
    }
    let fence = NonZeroU64::new(row.last_fence.checked_add(1).ok_or_else(|| {
        SessionStoreError::capacity(StoreCapacityBound::LeaseFence, row.last_fence, 0)
    })?)
    .ok_or(SessionStoreError::corrupt())?;
    let value = BranchLease::from_store(
        r.session(),
        r.owner(),
        fence,
        expiry(now, r.lease_seconds())?,
        r.lease_seconds(),
    );
    write_active(&tx, &value, None)?;
    bump(&tx)?;
    commit_claimed(tx, &mut b.holders, r.session(), fence)?;
    Ok(value)
}

/// Commits a transaction that grants `session` a lease under `fence`, holding
/// the branch's host-local claim from before the commit.
///
/// Claimed first, so the lock and the lease become visible together. Claiming
/// afterwards would leave a window in which the lease is live but its lock is
/// not yet held, which is exactly the state a concurrent reclaim reads as a
/// dead holder. A grant that fails to commit gives its claim back.
fn commit_claimed(
    tx: Transaction<'_>,
    holders: &mut crate::holder::HolderClaims,
    session: SessionRef,
    fence: NonZeroU64,
) -> Result<(), SessionStoreError> {
    holders.claim(session, fence);
    tx.commit().map_err(|error| {
        holders.release(session);
        map_sqlite(error)
    })
}
fn renew(b: &mut Backend, l: &BranchLease) -> Result<BranchLease, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, l.session().session_id())?;
    let row = branch_row(&tx, l.session())?.ok_or(SessionStoreError::NotFound)?;
    forfeit_if_superseded(&mut b.holders, &row, l);
    require_lease_domain(l, now)?;
    if row.previous.as_ref() == Some(l) {
        let active = row.active.ok_or(SessionStoreError::corrupt())?;
        require_lease_domain(&active, now)?;
        let out = if expired(&active, now)? {
            Err(SessionStoreError::LeaseExpired)
        } else {
            Ok(active)
        };
        tx.commit().map_err(map_sqlite)?;
        return out;
    }
    require_lease(&row, l, now)?;
    let expires_at = expiry(now, l.lease_seconds())?;
    if expires_at.same_domain_cmp(l.expires_at())? != std::cmp::Ordering::Greater {
        tx.commit().map_err(map_sqlite)?;
        return Ok(l.clone());
    }
    let value = BranchLease::from_store(
        l.session(),
        l.owner(),
        l.fence(),
        expires_at,
        l.lease_seconds(),
    );
    write_active(&tx, &value, Some(l))?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(value)
}
/// Extends an expired lease under its own fence when nothing has happened to
/// the branch since: it still names exactly this lease, no acquire has bumped
/// the fence, this store still holds the host lock it took with the lease, and
/// the clock domain is unchanged. Any acquire replaces `active` and bumps
/// `last_fence` first, so a revival and an acquire cannot both succeed.
fn revive(b: &mut Backend, l: &BranchLease) -> Result<BranchLease, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, l.session().session_id())?;
    let row = branch_row(&tx, l.session())?.ok_or(SessionStoreError::NotFound)?;
    forfeit_if_superseded(&mut b.holders, &row, l);
    require_lease_domain(l, now)?;
    if row.active.as_ref() != Some(l) || row.last_fence != l.fence().get() {
        tx.commit().map_err(map_sqlite)?;
        return Err(SessionStoreError::FenceSuperseded);
    }
    if !expired(l, now)? {
        drop(tx);
        return renew(b, l);
    }
    if !b.holders.holds(l.session()) {
        tx.commit().map_err(map_sqlite)?;
        return Err(SessionStoreError::FenceSuperseded);
    }
    let value = BranchLease::from_store(
        l.session(),
        l.owner(),
        l.fence(),
        expiry(now, l.lease_seconds())?,
        l.lease_seconds(),
    );
    write_active(&tx, &value, Some(l))?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(value)
}
fn release_many(b: &mut Backend, leases: &[BranchLease]) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    let mut changed = false;
    for lease in leases {
        live_session(&tx, lease.session().session_id())?;
        let row = branch_row(&tx, lease.session())?.ok_or(SessionStoreError::NotFound)?;
        forfeit_if_superseded(&mut b.holders, &row, lease);
        require_lease_domain(lease, now)?;
        if row.released.as_ref() == Some(lease) && row.active.is_none() {
            continue;
        }
        require_current_lease(&row, lease, now)?;
        mark_released(&tx, lease)?;
        feed::append(
            &tx,
            lease.session(),
            FeedChange::LeaseReleased {
                head: row.head.checkpoint().checkpoint_id(),
                fence: lease.fence().get(),
            },
            &[],
        )?;
        changed = true;
    }
    if changed {
        bump(&tx)?;
    }
    tx.commit().map_err(map_sqlite)?;
    for lease in leases {
        b.holders.release(lease.session());
    }
    let sessions = leases
        .iter()
        .map(|lease| lease.session().session_id())
        .collect::<BTreeSet<_>>();
    for session_id in sessions {
        feed::trim(b, session_id);
    }
    Ok(())
}

/// Drops this store's claim for `lease` once another acquire has replaced it,
/// so a superseded holder stops pinning a lock that no longer proves anything.
fn forfeit_if_superseded(
    holders: &mut crate::holder::HolderClaims,
    row: &BranchRow,
    lease: &BranchLease,
) {
    if row.last_fence != lease.fence().get() {
        holders.forfeit(lease.session(), lease.fence());
    }
}

fn mark_released(tx: &Transaction<'_>, l: &BranchLease) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE branches SET active_owner=NULL,active_fence=NULL,active_time=NULL,active_ns=NULL,
        active_duration=NULL,previous_time=NULL,previous_ns=NULL,released_owner=?3,released_fence=?4,
        released_time=?5,released_ns=?6,released_duration=?7 WHERE session_id=?1 AND branch_id=?2",
        params![id(l.session().session_id()), id(l.session().branch_id()), id(l.owner()), u64b(l.fence().get()), lease_time_bytes(l.expires_at()), i64::from(l.expires_at().nanoseconds()), u64b(l.lease_seconds().get())],
    ).map_err(map_sqlite)?;
    Ok(())
}

fn release(b: &mut Backend, l: &BranchLease) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, l.session().session_id())?;
    let row = branch_row(&tx, l.session())?.ok_or(SessionStoreError::NotFound)?;
    forfeit_if_superseded(&mut b.holders, &row, l);
    require_lease_domain(l, now)?;
    if row.released.as_ref() == Some(l) && row.active.is_none() {
        tx.commit().map_err(map_sqlite)?;
        b.holders.release(l.session());
        return Ok(());
    }
    require_current_lease(&row, l, now)?;
    mark_released(&tx, l)?;
    feed::append(
        &tx,
        l.session(),
        FeedChange::LeaseReleased {
            head: row.head.checkpoint().checkpoint_id(),
            fence: l.fence().get(),
        },
        &[],
    )?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    // The lock and the lease end together, so a released branch is not left
    // looking like one whose holder died.
    b.holders.release(l.session());
    Ok(())
}
fn write_active(
    tx: &Transaction<'_>,
    l: &BranchLease,
    previous: Option<&BranchLease>,
) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE branches SET last_fence=?3,active_owner=?4,active_fence=?3,active_time=?5,
        active_ns=?6,active_duration=?7,previous_time=?8,previous_ns=?9,released_owner=NULL,
        released_fence=NULL,released_time=NULL,released_ns=NULL,released_duration=NULL
        WHERE session_id=?1 AND branch_id=?2",
        params![
            id(l.session().session_id()),
            id(l.session().branch_id()),
            u64b(l.fence().get()),
            id(l.owner()),
            lease_time_bytes(l.expires_at()),
            i64::from(l.expires_at().nanoseconds()),
            u64b(l.lease_seconds().get()),
            previous.map(|x| lease_time_bytes(x.expires_at())),
            previous.map(|x| i64::from(x.expires_at().nanoseconds()))
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}
fn require_lease(
    row: &BranchRow,
    l: &BranchLease,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    require_current_lease(row, l, now)?;
    if expired(l, now)? {
        return Err(SessionStoreError::LeaseExpired);
    }
    Ok(())
}

fn require_current_lease(
    row: &BranchRow,
    l: &BranchLease,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    require_lease_domain(l, now)?;
    let active = row
        .active
        .as_ref()
        .ok_or(SessionStoreError::FenceSuperseded)?;
    if active != l {
        return Err(SessionStoreError::FenceSuperseded);
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "the single SQLite transaction keeps validate-then-publish atomicity auditable"
)]
fn commit(
    backend: &mut Backend,
    request: &CommitCheckpoint,
) -> Result<CommitReceipt, SessionStoreError> {
    commit_impl(backend, request, false).map(|(receipt, _)| receipt)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CommitExecution {
    transaction_begin_count: u64,
    transaction_commit_count: u64,
    transaction_elapsed_ns: u64,
    commit_call_elapsed_ns: u64,
    inserted: InsertEvidence,
}

#[cfg(feature = "perf-instrumentation")]
fn commit_observed(
    backend: &mut Backend,
    request: &CommitCheckpoint,
) -> Result<(CommitReceipt, CommitExecution), SessionStoreError> {
    commit_impl(backend, request, true)
}

#[allow(
    clippy::too_many_lines,
    reason = "the single SQLite transaction keeps validate-then-publish atomicity auditable"
)]
fn commit_impl(
    backend: &mut Backend,
    request: &CommitCheckpoint,
    observed: bool,
) -> Result<(CommitReceipt, CommitExecution), SessionStoreError> {
    #[cfg(not(feature = "perf-instrumentation"))]
    let _ = observed;
    backend.require_open()?;
    let reference = bundle_ref(request.checkpoint());
    let candidate = candidate(request.checkpoint())?;
    #[cfg(feature = "perf-instrumentation")]
    let transaction_started = observed.then(Instant::now);
    let tx = backend
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, backend.domain)?;
    let now = sample_clock(backend.clock.as_ref(), backend.domain)?;
    if let Some(old) = load_commit_receipt(&tx, reference.checkpoint_id())? {
        let out = if commit_matches(
            &old,
            request,
            reference,
            candidate.checkpoint.checkpoint_commit_identity(),
        ) {
            Ok(old.receipt())
        } else {
            Err(SessionStoreError::IdentityConflict)
        };
        #[cfg(feature = "perf-instrumentation")]
        let commit_started = observed.then(Instant::now);
        tx.commit().map_err(map_sqlite)?;
        #[cfg(feature = "perf-instrumentation")]
        let execution = CommitExecution {
            transaction_begin_count: 1,
            transaction_commit_count: 1,
            transaction_elapsed_ns: transaction_started.map_or(0, elapsed_ns),
            commit_call_elapsed_ns: commit_started.map_or(0, elapsed_ns),
            inserted: InsertEvidence::default(),
        };
        #[cfg(not(feature = "perf-instrumentation"))]
        let execution = CommitExecution::default();
        return out.map(|receipt| (receipt, execution));
    }
    if commit_history_pruned(&tx, reference.checkpoint_id())? {
        return Err(SessionStoreError::Pruned);
    }
    let session = live_session(&tx, request.expected().session().session_id())?;
    let row = branch_row(&tx, request.expected().session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&row, request.lease(), now)?;
    if row.head != request.expected() {
        return Err(SessionStoreError::Conflict);
    }
    let expected = checkpoint_row(&tx, request.expected().checkpoint().checkpoint_id())?
        .ok_or(SessionStoreError::corrupt())?;
    let next = request
        .expected()
        .revision()
        .checked_next()
        .ok_or_else(|| {
            SessionStoreError::capacity(
                StoreCapacityBound::BranchRevision,
                request.expected().revision().get(),
                0,
            )
        })?;
    let topology = candidate.checkpoint.manifest().topology();
    if candidate.base != Some(expected.reference.root())
        || expected.reference != request.expected().checkpoint()
        || topology.session_id != request.expected().session().session_id()
        || topology.branch_id != request.expected().session().branch_id()
        || topology.checkpoint_id != reference.checkpoint_id()
        || topology.parent_checkpoint_id != Some(expected.reference.checkpoint_id())
        || topology.branch_revision != next.get()
        || topology.fork != row.lineage
        || topology.lease_fence != request.lease().fence().get()
        || candidate.checkpoint.manifest().durability() != session.durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointCommitTopology,
        ));
    }
    let inserted = insert_candidate(&tx, &candidate, backend.limits, request.admission())?;
    let head = BranchHead::from_store(request.expected().session(), next, reference);
    tx.execute(
        "UPDATE branches SET revision=?3,checkpoint_id=?4 WHERE session_id=?1 AND branch_id=?2",
        params![
            id(head.session().session_id()),
            id(head.session().branch_id()),
            u64b(next.get()),
            id(reference.checkpoint_id())
        ],
    )
    .map_err(map_sqlite)?;
    let feed_seq = feed::append(
        &tx,
        request.expected().session(),
        FeedChange::Advanced {
            parent: request.expected().checkpoint().checkpoint_id(),
            checkpoint: reference.checkpoint_id(),
            revision: next,
            head: feed::head_kind(candidate.checkpoint.kind()),
        },
        request.feed_facts(),
    )?;
    let lease = request.lease();
    let receipt = CommitReceipt::from_store(reference, request.expected(), head, feed_seq);
    let committed = CommittedCheckpoint::from_store(
        CommitOperation::from_store(
            lease.clone(),
            request.expected(),
            reference,
            candidate.checkpoint.checkpoint_commit_identity(),
        ),
        receipt,
    );
    let receipt_integrity = commit_receipt_integrity(&committed)?;
    exactly_one(
        tx.execute(
            "INSERT INTO commit_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                id(reference.checkpoint_id()),
                id(lease.session().session_id()),
                id(lease.session().branch_id()),
                id(lease.owner()),
                u64b(lease.fence().get()),
                lease_time_bytes(lease.expires_at()),
                i64::from(lease.expires_at().nanoseconds()),
                u64b(lease.lease_seconds().get()),
                u64b(request.expected().revision().get()),
                id(request.expected().checkpoint().checkpoint_id()),
                u64b(next.get()),
                receipt_integrity,
                i64n(feed_seq.get())?
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    bump(&tx)?;
    #[cfg(test)]
    test_crash_cut("before-commit");
    #[cfg(test)]
    test_crash_signal("async-in-commit");
    #[cfg(feature = "perf-instrumentation")]
    let commit_started = observed.then(Instant::now);
    tx.commit().map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut("async-in-commit");
    backend.checkpoints.seed(
        request.expected().session(),
        reference,
        request.checkpoint(),
        Some(request.expected().checkpoint().checkpoint_id()),
    );
    #[cfg(feature = "perf-instrumentation")]
    let execution = CommitExecution {
        transaction_begin_count: 1,
        transaction_commit_count: 1,
        transaction_elapsed_ns: transaction_started.map_or(0, elapsed_ns),
        commit_call_elapsed_ns: commit_started.map_or(0, elapsed_ns),
        inserted,
    };
    #[cfg(not(feature = "perf-instrumentation"))]
    let execution = CommitExecution {
        inserted,
        ..CommitExecution::default()
    };
    #[cfg(test)]
    test_crash_cut("post-commit-pre-reply");
    Ok((receipt, execution))
}
fn recover_commit(
    c: &Connection,
    idv: CheckpointId,
) -> Result<CommittedCheckpoint, SessionStoreError> {
    match load_commit_receipt(c, idv)? {
        Some(committed) => Ok(committed),
        None if commit_history_pruned(c, idv)? => Err(SessionStoreError::Pruned),
        None => Err(SessionStoreError::NotFound),
    }
}
fn load_commit_receipt(
    c: &Connection,
    idv: CheckpointId,
) -> Result<Option<CommittedCheckpoint>, SessionStoreError> {
    c.query_row(
        "SELECT session_id,branch_id,owner,fence,expires_time,expires_ns,duration,
        expected_revision,expected_checkpoint,head_revision,receipt_integrity,feed_seq
        FROM commit_receipts WHERE checkpoint_id=?1",
        [id(idv)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Vec<u8>>(6)?,
                r.get::<_, Vec<u8>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Vec<u8>>(9)?,
                r.get::<_, Vec<u8>>(10)?,
                r.get::<_, i64>(11)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let s = SessionRef::new(parse(&v.0)?, parse(&v.1)?);
        let l = BranchLease::from_store(
            s,
            parse(&v.2)?,
            nz(&v.3)?,
            lease_time(&v.4, v.5)?,
            nz(&v.6)?,
        );
        let old = checkpoint_row(c, parse(&v.8)?)?.ok_or(SessionStoreError::corrupt())?;
        let new = checkpoint_row(c, idv)?.ok_or(SessionStoreError::corrupt())?;
        let previous_revision = BranchRevision::from_store(read_u64(&v.7)?);
        let head_revision = BranchRevision::from_store(read_u64(&v.9)?);
        if previous_revision.checked_next() != Some(head_revision)
            || old.selector != s
            || new.selector != s
            || old.revision != previous_revision
            || new.revision != head_revision
            || new.parent != Some(old.reference.checkpoint_id())
            || new.lineage != old.lineage
            || new.fence != l.fence().get()
            || new.durability != old.durability
        {
            return Err(SessionStoreError::corrupt());
        }
        let previous = BranchHead::from_store(s, previous_revision, old.reference);
        let head = BranchHead::from_store(s, head_revision, new.reference);
        let committed = CommittedCheckpoint::from_store(
            CommitOperation::from_store(l, previous, new.reference, new.commit),
            CommitReceipt::from_store(new.reference, previous, head, feed::receipt_seq(v.11)?),
        );
        if commit_receipt_integrity(&committed)? != v.10 {
            return Err(SessionStoreError::corrupt());
        }
        Ok(committed)
    })
    .transpose()
}
fn commit_matches(
    v: &CommittedCheckpoint,
    r: &CommitCheckpoint,
    x: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
) -> bool {
    v.operation().lease() == r.lease()
        && v.operation().expected() == r.expected()
        && v.operation().checkpoint() == x
        && v.operation().checkpoint_commit_identity() == checkpoint_commit_identity
}

// Keeping the full transaction linear makes its validate-then-publish boundary auditable.
#[allow(
    clippy::too_many_lines,
    reason = "the single transaction remains linear so validation and publication are auditable"
)]
fn fork(b: &mut Backend, r: &ForkBranch) -> Result<ForkReceipt, SessionStoreError> {
    b.require_open()?;
    super::validate_fork_semantic_request(r)?;
    let reference = bundle_ref(r.target());
    let x = candidate(r.target())?;
    let target = SessionRef::new(r.source().session().session_id(), r.target_branch());
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    if let Some(old) = load_fork(&tx, r.workflow_id())? {
        let out = if fork_matches(
            &old,
            r,
            reference,
            x.checkpoint.checkpoint_commit_identity(),
        ) {
            Ok(old)
        } else {
            Err(SessionStoreError::IdentityConflict)
        };
        tx.commit().map_err(map_sqlite)?;
        return out;
    }
    let expires = expiry(now, r.lease_seconds())?;
    let session = live_session(&tx, target.session_id())?;
    if session.revision() != r.expected_session_revision() {
        return Err(SessionStoreError::Conflict);
    }
    if branch_row(&tx, target)?.is_some() {
        return Err(SessionStoreError::IdentityConflict);
    }
    let count: u64 = tx
        .query_row(
            "SELECT count(*) FROM branches WHERE session_id=?1",
            [id(target.session_id())],
            |z| z.get::<_, i64>(0),
        )
        .map_err(map_sqlite)
        .and_then(nonnegative)?;
    enforce(
        StoreCapacityBound::BranchesPerSession,
        count + 1,
        b.limits.max_branches_per_session().get(),
    )?;
    let source = branch_row(&tx, r.source().session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&source, r.source_lease(), now)?;
    if source.head != r.source() {
        return Err(SessionStoreError::Conflict);
    }
    let source_cp = load_checkpoint(&tx, source.head.checkpoint().checkpoint_id()).map_err(
        |error| match error {
            SessionStoreError::NotFound => SessionStoreError::corrupt(),
            other => other,
        },
    )?;
    let source_checkpoint = source_cp.checkpoint();
    let t = x.checkpoint.manifest().topology();
    let lineage = ForkLineage {
        session_id: r.source().session().session_id(),
        branch_id: r.source().session().branch_id(),
        checkpoint_id: r.source().checkpoint().checkpoint_id(),
        branch_revision: r.source().revision().get(),
    };
    if source_checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || x.checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || x.base != Some(source_cp.reference().root())
        || t.session_id != target.session_id()
        || t.branch_id != target.branch_id()
        || t.checkpoint_id != reference.checkpoint_id()
        || t.parent_checkpoint_id != Some(source_cp.reference().checkpoint_id())
        || t.branch_revision != 0
        || t.fork != Some(lineage)
        || t.lease_fence != 1
        || x.checkpoint.manifest().durability() != session.durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::ForkCheckpointSemantics,
        ));
    }
    super::validate_branch_manifest_transition(source_checkpoint, &x.checkpoint, r.transition())?;
    let revision = r
        .expected_session_revision()
        .checked_next()
        .ok_or_else(|| {
            SessionStoreError::capacity(
                StoreCapacityBound::SessionRevision,
                r.expected_session_revision().get(),
                0,
            )
        })?;
    let lease_fence = NonZeroU64::MIN;
    let lease = BranchLease::from_store(target, r.owner(), lease_fence, expires, r.lease_seconds());
    let head = BranchHead::from_store(target, BranchRevision::INITIAL, reference);
    insert_candidate(&tx, &x, b.limits, r.admission())?;
    tx.execute(
        "UPDATE sessions SET revision=?2 WHERE session_id=?1",
        params![id(target.session_id()), u64b(revision.get())],
    )
    .map_err(map_sqlite)?;
    insert_branch(&tx, head, Some(r.source()), 1, Some(&lease))?;
    let feed_seq = feed::append(
        &tx,
        target,
        FeedChange::Branched {
            source: FeedLocator {
                session_id: r.source().session().session_id(),
                branch_id: r.source().session().branch_id(),
                checkpoint_id: r.source().checkpoint().checkpoint_id(),
            },
            target: reference.checkpoint_id(),
            revision,
            transition: match r.transition() {
                BranchTransitionMode::OrdinaryFork => FeedBranchTransition::Fork,
                BranchTransitionMode::Rebind => FeedBranchTransition::Rebind,
            },
        },
        r.feed_facts(),
    )?;
    let next = SessionHead::from_store(
        session.session_id(),
        session.initial_branch(),
        revision,
        session.created_at(),
        session.durability(),
        false,
    );
    let receipt = ForkReceipt::from_store(
        ForkOperation::from_store(
            r.workflow_id(),
            r.semantic_request(),
            r.expected_session_revision(),
            r.source(),
            target,
            r.transition(),
            reference,
            x.checkpoint.checkpoint_commit_identity(),
        ),
        session,
        next,
        r.source(),
        head,
        lease,
        feed_seq,
    );
    let receipt_integrity = fork_receipt_integrity(&receipt)?;
    exactly_one(
        tx.execute(
            "INSERT INTO fork_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,
        ?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
            params![
                workflow_id_bytes(r.workflow_id()),
                id(target.session_id()),
                id(target.branch_id()),
                id(r.source().session().session_id()),
                id(r.source().session().branch_id()),
                u64b(r.expected_session_revision().get()),
                u64b(r.source().revision().get()),
                i64::from(r.semantic_request().version()),
                r.semantic_request().bytes().to_vec(),
                id(r.source().checkpoint().checkpoint_id()),
                transition_tag(r.transition()),
                id(reference.checkpoint_id()),
                id(r.owner()),
                u64b(r.lease_seconds().get()),
                u64b(1),
                lease_time_bytes(expires),
                i64::from(expires.nanoseconds()),
                id(session.initial_branch()),
                u64b(session.created_at().seconds()),
                i64::from(session.created_at().nanoseconds()),
                durability_tag(session.durability())?,
                u64b(session.revision().get()),
                u64b(next.revision().get()),
                receipt_integrity,
                i64n(feed_seq.get())?
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    meta(&tx, 0, 1)?;
    commit_claimed(tx, &mut b.holders, target, lease_fence)?;
    b.checkpoints.seed(target, reference, r.target(), None);
    Ok(receipt)
}
fn recover_fork(b: &Backend, r: &RecoverFork) -> Result<ForkReceipt, SessionStoreError> {
    let receipt = load_fork(&b.connection, r.workflow_id())?.ok_or(SessionStoreError::NotFound)?;
    if !recover_fork_matches(&receipt, r) {
        return Err(SessionStoreError::IdentityConflict);
    }
    let source =
        branch_row(&b.connection, r.source().session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(
        &source,
        r.source_lease(),
        sample_clock(b.clock.as_ref(), b.domain)?,
    )?;
    Ok(receipt)
}

#[allow(
    clippy::too_many_lines,
    reason = "linear receipt decode keeps historical source/target validation auditable"
)]
fn load_fork(
    c: &Connection,
    workflow_id: SessionWorkflowId,
) -> Result<Option<ForkReceipt>, SessionStoreError> {
    c.query_row(
        "SELECT target_session,target_branch,source_session,source_branch,
        expected_session_revision,source_revision,semantic_request_version,semantic_request,
        source_checkpoint,transition,target_checkpoint,
        target_owner,target_duration,target_fence,target_expires_time,target_expires_ns,
        initial_branch,created_s,created_ns,durability,previous_session_revision,session_revision,
        receipt_integrity,feed_seq FROM fork_receipts WHERE workflow_id=?1",
        [workflow_id_bytes(workflow_id)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Vec<u8>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, String>(11)?,
                r.get::<_, Vec<u8>>(12)?,
                r.get::<_, Vec<u8>>(13)?,
                r.get::<_, Vec<u8>>(14)?,
                r.get::<_, i64>(15)?,
                r.get::<_, String>(16)?,
                r.get::<_, Vec<u8>>(17)?,
                r.get::<_, i64>(18)?,
                r.get::<_, i64>(19)?,
                r.get::<_, Vec<u8>>(20)?,
                r.get::<_, Vec<u8>>(21)?,
                r.get::<_, Vec<u8>>(22)?,
                r.get::<_, i64>(23)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let target = SessionRef::new(parse(&v.0)?, parse(&v.1)?);
        let source_ref = SessionRef::new(parse(&v.2)?, parse(&v.3)?);
        let expected_session_revision = SessionRevision::from_store(read_u64(&v.4)?);
        let source_revision = BranchRevision::from_store(read_u64(&v.5)?);
        let semantic_request = decode_digest(DigestDomain::RequestPlan, v.6, v.7)?;
        let source_checkpoint = load_checkpoint(c, parse(&v.8)?)?;
        let transition = decode_transition(v.9)?;
        let target_checkpoint = load_checkpoint(c, parse(&v.10)?)?;
        let owner: RunId = parse(&v.11)?;
        let duration = nz(&v.12)?;
        let lease = BranchLease::from_store(
            target,
            owner,
            nz(&v.13)?,
            lease_time(&v.14, v.15)?,
            duration,
        );
        let initial = parse(&v.16)?;
        let created = timestamp(&v.17, v.18)?;
        let durability = decode_durability(v.19)?;
        let previous_revision = SessionRevision::from_store(read_u64(&v.20)?);
        let session_revision = SessionRevision::from_store(read_u64(&v.21)?);
        let source =
            BranchHead::from_store(source_ref, source_revision, source_checkpoint.reference());
        let previous = SessionHead::from_store(
            target.session_id(),
            initial,
            previous_revision,
            created,
            durability,
            false,
        );
        let session = SessionHead::from_store(
            target.session_id(),
            initial,
            session_revision,
            created,
            durability,
            false,
        );
        let head = BranchHead::from_store(
            target,
            BranchRevision::INITIAL,
            target_checkpoint.reference(),
        );
        let receipt = ForkReceipt::from_store(
            ForkOperation::from_store(
                workflow_id,
                semantic_request,
                expected_session_revision,
                source,
                target,
                transition,
                target_checkpoint.reference(),
                target_checkpoint.checkpoint().checkpoint_commit_identity(),
            ),
            previous,
            session,
            source,
            head,
            lease,
            feed::receipt_seq(v.23)?,
        );
        let persisted_session =
            session_head(c, target.session_id())?.ok_or(SessionStoreError::corrupt())?;
        if persisted_session.initial_branch() != initial
            || persisted_session.created_at() != created
            || persisted_session.durability() != durability
            || fork_receipt_integrity(&receipt)? != v.22
        {
            return Err(SessionStoreError::corrupt());
        }
        super::validate_materialized_fork_receipt(
            &receipt,
            &source_checkpoint,
            &target_checkpoint,
        )?;
        Ok(receipt)
    })
    .transpose()
}
fn fork_matches(
    v: &ForkReceipt,
    r: &ForkBranch,
    x: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
) -> bool {
    let o = v.operation();
    o.workflow_id() == r.workflow_id()
        && o.semantic_request() == r.semantic_request()
        && o.expected_session_revision() == r.expected_session_revision()
        && o.source() == r.source()
        && o.target().branch_id() == r.target_branch()
        && o.transition() == r.transition()
        && o.checkpoint() == x
        && o.checkpoint_commit_identity() == checkpoint_commit_identity
        && r.target().base_root() == Some(o.source().checkpoint().root())
}

fn recover_fork_matches(v: &ForkReceipt, r: &RecoverFork) -> bool {
    let o = v.operation();
    o.workflow_id() == r.workflow_id()
        && o.source() == r.source()
        && o.target().branch_id() == r.target_branch()
        && o.transition() == r.transition()
        && o.semantic_request() == r.semantic_request()
}

fn load_branch(c: &Connection, s: SessionRef) -> Result<LoadedBranch, SessionStoreError> {
    let session = live_session(c, s.session_id())?;
    let row = branch_row(c, s)?.ok_or(SessionStoreError::NotFound)?;
    let checkpoint = load_checkpoint(c, row.head.checkpoint().checkpoint_id())?;
    if checkpoint.reference() != row.head.checkpoint() {
        return Err(SessionStoreError::corrupt());
    }
    with_loaded_horizon(
        c,
        s,
        LoadedBranch::from_store(session, row.head, checkpoint),
    )
}

fn with_loaded_horizon(
    c: &Connection,
    selector: SessionRef,
    loaded: LoadedBranch,
) -> Result<LoadedBranch, SessionStoreError> {
    Ok(match load_horizon(c, selector)? {
        Some(horizon) => loaded.with_horizon(horizon.revision),
        None => loaded,
    })
}
fn load_checkpoint(
    c: &Connection,
    idv: CheckpointId,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let Some(row) = checkpoint_row(c, idv)? else {
        return Err(if pruned_checkpoint(c, idv)? {
            SessionStoreError::Pruned
        } else {
            SessionStoreError::NotFound
        });
    };
    load_checkpoint_contents(c, idv, &row)
}

fn load_checkpoint_contents(
    c: &Connection,
    idv: CheckpointId,
    row: &CheckpointRow,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let root = row.reference.root();
    let mut children = load_reachable_children(c, root)?;
    let mut s=c.prepare("WITH RECURSIVE reach(kind,codec,digest) AS(
        VALUES(?1,?2,?3) UNION SELECT e.child_kind,e.child_codec,e.child_digest
        FROM runtime_object_edges e JOIN reach p ON e.parent_kind=p.kind
        AND e.parent_codec=p.codec AND e.parent_digest=p.digest)
        SELECT o.kind,o.codec,o.digest,p.bytes FROM reach JOIN runtime_objects o USING(kind,codec,digest) JOIN runtime_object_bytes p USING(id)
        ORDER BY o.kind,o.codec,o.digest").map_err(map_sqlite)?;
    let mut rows = s
        .query(params![
            kind_tag(root.kind()),
            i64::from(root.codec_revision()),
            root.digest().bytes().as_slice()
        ])
        .map_err(map_sqlite)?;
    let mut objects = Vec::new();
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        let a = decode_address(
            r.get(0).map_err(map_sqlite)?,
            r.get(1).map_err(map_sqlite)?,
            r.get(2).map_err(map_sqlite)?,
        )?;
        objects.push(
            RuntimeObject::from_store(
                a,
                r.get(3).map_err(map_sqlite)?,
                children.remove(&a).unwrap_or_default(),
            )
            .map_err(bundle_error)?,
        );
    }
    if !children.is_empty() {
        return Err(SessionStoreError::corrupt());
    }
    let bundle = CheckpointObjectBundle::from_store(idv, row.reference.integrity(), root, objects)
        .map_err(bundle_error)?;
    let checkpoint = bundle.validated_checkpoint().map_err(bundle_error)?;
    let t = checkpoint.manifest().topology();
    let key = semantic_key(checkpoint.manifest(), bundle.supplied_objects())?;
    // A parent reclaimed by retention leaves the sealed base as the only
    // record of what this checkpoint was built on.
    let expected_base = match row.parent {
        Some(parent) => match checkpoint_row(c, parent)? {
            Some(parent) => Some(parent.reference.root()),
            None if pruned_checkpoint(c, parent)? => row.base,
            None => return Err(SessionStoreError::corrupt()),
        },
        None => None,
    };
    if t.session_id != row.selector.session_id()
        || t.branch_id != row.selector.branch_id()
        || t.branch_revision != row.revision.get()
        || t.parent_checkpoint_id != row.parent
        || t.fork != row.lineage
        || t.lease_fence != row.fence
        || checkpoint.kind() != row.kind
        || checkpoint.manifest().durability() != row.durability
        || checkpoint.checkpoint_commit_identity() != row.commit
        || key != row.key
        || row.base != expected_base
    {
        return Err(SessionStoreError::corrupt());
    }
    StoredCheckpoint::from_store(row.reference, bundle).map_err(bundle_error)
}
fn list(
    c: &Connection,
    limits: MemoryStoreLimits,
    q: SessionQuery,
) -> Result<Page<SessionSummary>, SessionStoreError> {
    if q.limit().get() > limits.max_page_size().get() {
        return Err(SessionStoreError::capacity(
            StoreCapacityBound::PageSize,
            u64n(q.limit().get()),
            u64n(limits.max_page_size().get()),
        ));
    }
    let mut s = c
        .prepare(
            "SELECT session_id,initial_branch,revision,created_s,created_ns,durability,
        archived,(SELECT count(*) FROM branches b WHERE b.session_id=sessions.session_id)
        FROM sessions WHERE (?1 IS NULL OR session_id>?1) AND (?2=1 OR archived=0)
        ORDER BY session_id LIMIT ?3",
        )
        .map_err(map_sqlite)?;
    let mut rows = s
        .query(params![
            q.after_session().map(id),
            i64::from(q.includes_archived()),
            i64n(u64n(q.limit().get()) + 1)?
        ])
        .map_err(map_sqlite)?;
    let mut items = Vec::new();
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        let sid = parse(&r.get::<_, String>(0).map_err(map_sqlite)?)?;
        let h = SessionHead::from_store(
            sid,
            parse(&r.get::<_, String>(1).map_err(map_sqlite)?)?,
            SessionRevision::from_store(read_u64(&r.get::<_, Vec<u8>>(2).map_err(map_sqlite)?)?),
            timestamp(
                &r.get::<_, Vec<u8>>(3).map_err(map_sqlite)?,
                r.get(4).map_err(map_sqlite)?,
            )?,
            decode_durability(r.get(5).map_err(map_sqlite)?)?,
            boolv(r.get(6).map_err(map_sqlite)?)?,
        );
        items.push(SessionSummary::from_store(
            h,
            nonnegative(r.get(7).map_err(map_sqlite)?)?,
        ));
    }
    let more = items.len() > q.limit().get();
    if more {
        items.pop();
    }
    let next = if more {
        items.last().map(|x| x.head().session_id())
    } else {
        None
    };
    Ok(Page::from_store(items, next))
}
fn archive(b: &mut Backend, r: ArchiveSession) -> Result<ArchiveReceipt, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    if let Some(old) = load_archive(&tx, r.session_id())? {
        let out = if old.request() == r {
            Ok(old)
        } else {
            Err(SessionStoreError::Archived)
        };
        tx.commit().map_err(map_sqlite)?;
        return out;
    }
    let session = session_head(&tx, r.session_id())?.ok_or(SessionStoreError::NotFound)?;
    if session.is_archived() {
        return Err(SessionStoreError::Archived);
    }
    if session.revision() != r.expected_revision() {
        return Err(SessionStoreError::Conflict);
    }
    let mut s = tx
        .prepare("SELECT branch_id FROM branches WHERE session_id=?1")
        .map_err(map_sqlite)?;
    let ids = s
        .query_map([id(r.session_id())], |z| z.get::<_, String>(0))
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    drop(s);
    for value in ids {
        let row = branch_row(&tx, SessionRef::new(r.session_id(), parse(&value)?))?
            .ok_or(SessionStoreError::corrupt())?;
        if row.active.as_ref().map(|l| expired(l, now)).transpose()? == Some(false) {
            return Err(SessionStoreError::LeaseHeld);
        }
    }
    let revision = r.expected_revision().checked_next().ok_or_else(|| {
        SessionStoreError::capacity(
            StoreCapacityBound::SessionRevision,
            r.expected_revision().get(),
            0,
        )
    })?;
    let archived = SessionHead::from_store(
        session.session_id(),
        session.initial_branch(),
        revision,
        session.created_at(),
        session.durability(),
        true,
    );
    tx.execute(
        "UPDATE sessions SET revision=?2,archived=1 WHERE session_id=?1",
        params![id(r.session_id()), u64b(revision.get())],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "UPDATE branches SET active_owner=NULL,active_fence=NULL,active_time=NULL,active_ns=NULL,
        active_duration=NULL,previous_time=NULL,previous_ns=NULL WHERE session_id=?1",
        [id(r.session_id())],
    )
    .map_err(map_sqlite)?;
    feed::append(
        &tx,
        SessionRef::new(r.session_id(), session.initial_branch()),
        FeedChange::Archived { revision },
        &[],
    )?;
    let receipt = ArchiveReceipt::from_store(r, session, archived);
    let receipt_integrity = archive_receipt_integrity(receipt)?;
    exactly_one(
        tx.execute(
            "INSERT INTO archive_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id(session.session_id()),
                id(session.initial_branch()),
                u64b(session.created_at().seconds()),
                i64::from(session.created_at().nanoseconds()),
                durability_tag(session.durability())?,
                u64b(session.revision().get()),
                u64b(revision.get()),
                receipt_integrity
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(receipt)
}
fn load_archive(
    c: &Connection,
    sid: SessionId,
) -> Result<Option<ArchiveReceipt>, SessionStoreError> {
    c.query_row(
        "SELECT initial_branch,created_s,created_ns,durability,expected_revision,
        session_revision,receipt_integrity FROM archive_receipts WHERE session_id=?1",
        [id(sid)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, Vec<u8>>(6)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let initial = parse(&v.0)?;
        let created = timestamp(&v.1, v.2)?;
        let d = decode_durability(v.3)?;
        let expected = SessionRevision::from_store(read_u64(&v.4)?);
        let next = SessionRevision::from_store(read_u64(&v.5)?);
        let receipt = ArchiveReceipt::from_store(
            ArchiveSession::new(sid, expected),
            SessionHead::from_store(sid, initial, expected, created, d, false),
            SessionHead::from_store(sid, initial, next, created, d, true),
        );
        if expected.checked_next() != Some(next)
            || session_head(c, sid)? != Some(receipt.session())
            || archive_receipt_integrity(receipt)? != v.6
        {
            return Err(SessionStoreError::corrupt());
        }
        Ok(receipt)
    })
    .transpose()
}

/// A checkpoint of another session whose parent or fork source is in session
/// `?1`.
const SESSION_FOREIGN_DEPENDENCY: &str = "SELECT 1 FROM checkpoints c WHERE c.session_id<>?1 AND (c.parent_id IN (SELECT checkpoint_id FROM checkpoints WHERE session_id=?1) OR c.fork_checkpoint IN (SELECT checkpoint_id FROM checkpoints WHERE session_id=?1)) LIMIT 1";

/// Session `?1`'s dependent rows, children before parents; its checkpoints
/// and session row go after their released roots are queued.
const SESSION_ROW_DELETES: [&str; 9] = [
    "DELETE FROM fork_receipts WHERE source_session=?1",
    "DELETE FROM fork_receipts WHERE target_session=?1",
    "DELETE FROM commit_receipts WHERE session_id=?1",
    "DELETE FROM archive_receipts WHERE session_id=?1",
    "DELETE FROM create_receipts WHERE session_id=?1",
    "DELETE FROM branch_horizons WHERE session_id=?1",
    "DELETE FROM pruned_checkpoints WHERE session_id=?1",
    "DELETE FROM branches WHERE session_id=?1",
    "DELETE FROM session_feed WHERE session_id=?1",
];

#[allow(
    clippy::too_many_lines,
    reason = "the one semantic deletion transaction stays linear for authority review"
)]
fn delete(b: &mut Backend, r: DeleteSession) -> Result<DeleteReceipt, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    if deletion_exists(&tx, r.session_id())? {
        let receipt = load_delete(&tx, r)?;
        tx.commit().map_err(map_sqlite)?;
        return Ok(receipt);
    }
    let archived = session_head(&tx, r.session_id())?.ok_or(SessionStoreError::NotFound)?;
    if !archived.is_archived() {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::SessionNotArchived,
        ));
    }
    if archived.revision() != r.expected_revision() {
        return Err(SessionStoreError::Conflict);
    }
    let branches = count_where(&tx, "branches", r.session_id())?;
    let checkpoints = count_where(&tx, "checkpoints", r.session_id())?;
    let foreign_dependency: Option<i64> = tx
        .query_row(SESSION_FOREIGN_DEPENDENCY, [id(r.session_id())], |row| {
            row.get(0)
        })
        .optional()
        .map_err(map_sqlite)?;
    if foreign_dependency.is_some() {
        return Err(SessionStoreError::corrupt());
    }

    for statement in SESSION_ROW_DELETES {
        tx.execute(statement, [id(r.session_id())])
            .map_err(map_sqlite)?;
    }
    capture_released_roots(&tx, "session_id=?1", &id(r.session_id()))?;
    tx.execute(
        "DELETE FROM checkpoints WHERE session_id=?1",
        [id(r.session_id())],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "DELETE FROM sessions WHERE session_id=?1",
        [id(r.session_id())],
    )
    .map_err(map_sqlite)?;

    let receipt = DeleteReceipt::from_store(r, archived, checkpoints);
    let integrity = delete_receipt_integrity(receipt)?;
    tx.execute(
        "INSERT INTO delete_tombstones VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            id(r.session_id()),
            id(archived.initial_branch()),
            u64b(archived.created_at().seconds()),
            i64::from(archived.created_at().nanoseconds()),
            durability_tag(archived.durability())?,
            u64b(archived.revision().get()),
            i64n(checkpoints)?,
            integrity
        ],
    )
    .map_err(map_sqlite)?;
    tx.execute("UPDATE store_meta SET sessions=sessions-1,branches=branches-?1,checkpoints=checkpoints-?2,semantic_mutations=semantic_mutations+1,tombstones=tombstones+1 WHERE id=1", params![i64n(branches)?,i64n(checkpoints)?]).map_err(map_sqlite)?;
    tx.commit().map_err(map_sqlite)?;
    b.gc_pending = true;
    Ok(receipt)
}

/// Checkpoints of branch `?1`/`?2` strictly below revision `?3` that no
/// receipt, fork, other branch or foreign child still names.
const PRUNE_CANDIDATES: &str = "SELECT checkpoint_id FROM checkpoints c
     WHERE c.session_id=?1 AND c.branch_id=?2 AND c.revision<?3
     AND NOT EXISTS (SELECT 1 FROM create_receipts r
         WHERE r.session_id=?1 AND r.checkpoint_id=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM fork_receipts f WHERE f.source_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM fork_receipts f WHERE f.target_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM checkpoints o WHERE o.fork_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM branches b WHERE b.fork_checkpoint=c.checkpoint_id)
     AND NOT EXISTS (SELECT 1 FROM checkpoints o WHERE o.parent_id=c.checkpoint_id
         AND (o.session_id<>?1 OR o.branch_id<>?2))
     ORDER BY checkpoint_id";

#[allow(
    clippy::too_many_lines,
    reason = "the one prune transaction stays linear so its retained set is auditable"
)]
fn prune(b: &mut Backend, r: &PruneBranch) -> Result<PruneReceipt, SessionStoreError> {
    b.require_open()?;
    let selector = r.expected().session();
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    live_session(&tx, selector.session_id())?;
    let row = branch_row(&tx, selector)?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&row, r.lease(), now)?;
    if row.head != r.expected() {
        return Err(SessionStoreError::Conflict);
    }
    let head = checkpoint_row(&tx, row.head.checkpoint().checkpoint_id())?
        .ok_or(SessionStoreError::corrupt())?;
    if head.kind != RuntimeCheckpointKind::Quiescent {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::PruneHeadNotQuiescent,
        ));
    }
    let previous = load_horizon(&tx, selector)?;
    let unchanged = PruneReceipt::from_store(
        row.head,
        previous
            .as_ref()
            .map_or(BranchRevision::INITIAL, |h| h.revision),
        0,
    );
    // The head's parent is the horizon: everything strictly below it that no
    // receipt, fork, other branch or the runtime still names is superseded.
    let (Some(horizon_revision), Some(horizon_checkpoint)) =
        (row.head.revision().get().checked_sub(1), head.parent)
    else {
        tx.commit().map_err(map_sqlite)?;
        return Ok(unchanged);
    };
    let mut statement = tx.prepare(PRUNE_CANDIDATES).map_err(map_sqlite)?;
    let superseded = statement
        .query_map(
            params![
                id(selector.session_id()),
                id(selector.branch_id()),
                u64b(horizon_revision)
            ],
            |z| z.get::<_, String>(0),
        )
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?
        .iter()
        .map(|value| parse::<CheckpointId>(value))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|checkpoint| !r.retain().contains(checkpoint))
        .collect::<Vec<_>>();
    drop(statement);
    let pruned = u64n(superseded.len());
    if pruned == 0 || pruned < r.minimum() {
        tx.commit().map_err(map_sqlite)?;
        return Ok(unchanged);
    }
    tx.execute(
        "DELETE FROM commit_receipts WHERE checkpoint_id IN
         (SELECT checkpoint_id FROM checkpoints WHERE session_id=?1 AND branch_id=?2
          AND revision<=?3)",
        params![
            id(selector.session_id()),
            id(selector.branch_id()),
            u64b(horizon_revision)
        ],
    )
    .map_err(map_sqlite)?;
    for checkpoint in &superseded {
        tx.execute(
            "INSERT INTO pruned_checkpoints VALUES(?1,?2,?3)",
            params![
                id(checkpoint),
                id(selector.session_id()),
                id(selector.branch_id())
            ],
        )
        .map_err(map_sqlite)?;
    }
    let total = |field: fn(&BranchHorizon) -> u64, delta: u64| {
        previous
            .as_ref()
            .map_or(0, field)
            .checked_add(delta)
            .ok_or(SessionStoreError::corrupt())
    };
    // The horizon row moves to its new checkpoint before the old one can go.
    let horizon = BranchHorizon {
        selector,
        revision: BranchRevision::from_store(horizon_revision),
        checkpoint: horizon_checkpoint,
        pruned_checkpoints: total(|h| h.pruned_checkpoints, pruned)?,
    };
    write_horizon(&tx, &horizon)?;
    for checkpoint in &superseded {
        capture_released_roots(&tx, "checkpoint_id=?1", &id(checkpoint))?;
        exactly_one(
            tx.execute(
                "DELETE FROM checkpoints WHERE checkpoint_id=?1",
                [id(checkpoint)],
            )
            .map_err(map_sqlite)?,
        )?;
    }
    #[cfg(test)]
    test_crash_cut("prune-after-enqueue");

    feed::append(
        &tx,
        selector,
        FeedChange::Pruned {
            head: row.head.checkpoint().checkpoint_id(),
            horizon: horizon.revision,
        },
        &[],
    )?;
    tx.execute("UPDATE store_meta SET checkpoints=checkpoints-?1,semantic_mutations=semantic_mutations+1 WHERE id=1", [i64n(pruned)?]).map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut("prune-before-commit");
    tx.commit().map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut("prune-after-commit");
    // Returning free pages is scheduled after the authoritative commit, not
    // awaited on the prune acknowledgment path.
    b.gc_pending = true;
    b.vacuum_pending = true;
    b.wal_pending = true;
    Ok(PruneReceipt::from_store(row.head, horizon.revision, pruned))
}

/// Returns up to `pages` free pages to the file system. The pragma frees one
/// page per result row, so every row must be stepped.
fn incremental_vacuum(c: &Connection, pages: u32) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(&format!("PRAGMA incremental_vacuum({pages})"))
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while rows.next().map_err(map_sqlite)?.is_some() {}
    Ok(())
}

/// One branch's recorded history horizon and cumulative reclaim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BranchHorizon {
    selector: SessionRef,
    revision: BranchRevision,
    checkpoint: CheckpointId,
    pruned_checkpoints: u64,
}

fn horizon_integrity(h: &BranchHorizon) -> Result<Vec<u8>, SessionStoreError> {
    receipt_integrity(
        b"branch-horizon-v1",
        &[
            session_ref_bytes(h.selector),
            h.revision.get().to_be_bytes().to_vec(),
            text_bytes(h.checkpoint),
            h.pruned_checkpoints.to_be_bytes().to_vec(),
        ],
    )
}

fn write_horizon(tx: &Transaction<'_>, h: &BranchHorizon) -> Result<(), SessionStoreError> {
    tx.execute(
        "INSERT INTO branch_horizons VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(session_id,branch_id) DO UPDATE SET horizon_revision=excluded.horizon_revision,
         horizon_checkpoint=excluded.horizon_checkpoint,
         pruned_checkpoints=excluded.pruned_checkpoints,
         integrity=excluded.integrity",
        params![
            id(h.selector.session_id()),
            id(h.selector.branch_id()),
            u64b(h.revision.get()),
            id(h.checkpoint),
            i64n(h.pruned_checkpoints)?,
            horizon_integrity(h)?
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

fn decode_horizon(
    selector: SessionRef,
    revision: &[u8],
    checkpoint: &str,
    count: i64,
    integrity: &[u8],
) -> Result<BranchHorizon, SessionStoreError> {
    let horizon = BranchHorizon {
        selector,
        revision: BranchRevision::from_store(read_u64(revision)?),
        checkpoint: parse(checkpoint)?,
        pruned_checkpoints: nonnegative(count)?,
    };
    if horizon_integrity(&horizon)? != integrity {
        return Err(SessionStoreError::corrupt());
    }
    Ok(horizon)
}

fn load_horizon(
    c: &Connection,
    selector: SessionRef,
) -> Result<Option<BranchHorizon>, SessionStoreError> {
    c.query_row(
        "SELECT horizon_revision,horizon_checkpoint,pruned_checkpoints,integrity FROM branch_horizons
         WHERE session_id=?1 AND branch_id=?2",
        params![id(selector.session_id()), id(selector.branch_id())],
        |z| {
            Ok((
                z.get::<_, Vec<u8>>(0)?,
                z.get::<_, String>(1)?,
                z.get::<_, i64>(2)?,
                z.get::<_, Vec<u8>>(3)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| decode_horizon(selector, &v.0, &v.1, v.2, &v.3))
    .transpose()
}

fn pruned_checkpoint(c: &Connection, checkpoint: CheckpointId) -> Result<bool, SessionStoreError> {
    c.query_row(
        "SELECT 1 FROM pruned_checkpoints WHERE checkpoint_id=?1",
        [id(checkpoint)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(map_sqlite)
}

/// Whether retention reclaimed this checkpoint's commit receipt: the
/// checkpoint itself was pruned, or it is a retained checkpoint at or below
/// its branch horizon.
fn commit_history_pruned(
    c: &Connection,
    checkpoint: CheckpointId,
) -> Result<bool, SessionStoreError> {
    if pruned_checkpoint(c, checkpoint)? {
        return Ok(true);
    }
    c.query_row(
        "SELECT 1 FROM checkpoints k JOIN branch_horizons h
         ON h.session_id=k.session_id AND h.branch_id=k.branch_id
         WHERE k.checkpoint_id=?1 AND k.revision<>zeroblob(8) AND k.revision<=h.horizon_revision",
        [id(checkpoint)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(map_sqlite)
}

fn load_delete(c: &Connection, r: DeleteSession) -> Result<DeleteReceipt, SessionStoreError> {
    let row = c.query_row("SELECT initial_branch,created_s,created_ns,durability,archived_revision,reclaimed_checkpoints,receipt_integrity FROM delete_tombstones WHERE session_id=?1", [id(r.session_id())], |z| Ok((z.get::<_,String>(0)?,z.get::<_,Vec<u8>>(1)?,z.get::<_,i64>(2)?,z.get::<_,i64>(3)?,z.get::<_,Vec<u8>>(4)?,z.get::<_,i64>(5)?,z.get::<_,Vec<u8>>(6)?))).optional().map_err(map_sqlite)?.ok_or(SessionStoreError::NotFound)?;
    let revision = SessionRevision::from_store(read_u64(&row.4)?);
    if revision == SessionRevision::INITIAL || revision != r.expected_revision() {
        return Err(SessionStoreError::Conflict);
    }
    if session_head(c, r.session_id())?.is_some() {
        return Err(SessionStoreError::corrupt());
    }
    let archived = SessionHead::from_store(
        r.session_id(),
        parse(&row.0)?,
        revision,
        timestamp(&row.1, row.2)?,
        decode_durability(row.3)?,
        true,
    );
    let receipt = DeleteReceipt::from_store(r, archived, nonnegative(row.5)?);
    if delete_receipt_integrity(receipt)? != row.6 {
        return Err(SessionStoreError::corrupt());
    }
    Ok(receipt)
}

fn deletion_exists(c: &Connection, session_id: SessionId) -> Result<bool, SessionStoreError> {
    c.query_row(
        "SELECT 1 FROM delete_tombstones WHERE session_id=?1",
        [id(session_id)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(map_sqlite)
}
fn require_unused_session_identity(
    c: &Connection,
    session_id: SessionId,
) -> Result<(), SessionStoreError> {
    if session_head(c, session_id)?.is_some() || deletion_exists(c, session_id)? {
        return Err(SessionStoreError::IdentityConflict);
    }
    Ok(())
}
fn count_where(c: &Connection, table: &str, sid: SessionId) -> Result<u64, SessionStoreError> {
    c.query_row(
        &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
        [id(sid)],
        |r| r.get::<_, i64>(0),
    )
    .map_err(map_sqlite)
    .and_then(nonnegative)
}

/// Queues roots in the same transaction that removes their checkpoint authority.
fn capture_released_roots(
    tx: &Transaction<'_>,
    condition: &str,
    parameter: &str,
) -> Result<(), SessionStoreError> {
    tx.execute(&format!("INSERT OR IGNORE INTO gc_candidates SELECT root_kind,root_codec,root_digest FROM checkpoints WHERE {condition}"), [parameter]).map_err(map_sqlite)?;
    Ok(())
}

fn lease(
    session: SessionRef,
    owner: Option<&str>,
    fence: Option<&[u8]>,
    expires: Option<&[u8]>,
    nanoseconds: Option<i64>,
    duration: Option<&[u8]>,
) -> Result<Option<BranchLease>, SessionStoreError> {
    match (owner, fence, expires, nanoseconds, duration) {
        (None, None, None, None, None) => Ok(None),
        (Some(owner), Some(fence), Some(expires), Some(nanoseconds), Some(duration)) => {
            Ok(Some(BranchLease::from_store(
                session,
                parse(owner)?,
                nz(fence)?,
                lease_time(expires, nanoseconds)?,
                nz(duration)?,
            )))
        }
        _ => Err(SessionStoreError::corrupt()),
    }
}
fn lineage(
    session: Option<&str>,
    branch: Option<&str>,
    checkpoint: Option<&str>,
    revision: Option<&[u8]>,
) -> Result<Option<ForkLineage>, SessionStoreError> {
    match (session, branch, checkpoint, revision) {
        (None, None, None, None) => Ok(None),
        (Some(session), Some(branch), Some(checkpoint), Some(revision)) => Ok(Some(ForkLineage {
            session_id: parse(session)?,
            branch_id: parse(branch)?,
            checkpoint_id: parse(checkpoint)?,
            branch_revision: read_u64(revision)?,
        })),
        _ => Err(SessionStoreError::corrupt()),
    }
}
fn meta(tx: &Transaction<'_>, sessions: u64, branches: u64) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE store_meta SET sessions=sessions+?1,branches=branches+?2,
        semantic_mutations=semantic_mutations+1 WHERE id=1",
        params![i64n(sessions)?, i64n(branches)?],
    )
    .map_err(map_sqlite)?;
    Ok(())
}
fn bump(tx: &Transaction<'_>) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE store_meta SET semantic_mutations=semantic_mutations+1 WHERE id=1",
        [],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

fn checkpoint_wal(c: &Connection, truncate: bool) -> Result<(), SessionStoreError> {
    let sql = if truncate {
        "PRAGMA wal_checkpoint(TRUNCATE)"
    } else {
        "PRAGMA wal_checkpoint(PASSIVE)"
    };
    let busy: i64 = c.query_row(sql, [], |r| r.get(0)).map_err(map_sqlite)?;
    if busy == 0 {
        Ok(())
    } else {
        Err(SessionStoreError::BackendBusy)
    }
}
fn report() -> DurabilityReport {
    DurabilityReport::from_store(RuntimeDurability::Required, 0, None)
}
fn durability_tag(d: RuntimeDurability) -> Result<u8, SessionStoreError> {
    match d {
        RuntimeDurability::Required => Ok(1),
        RuntimeDurability::BestEffort => Ok(2),
        RuntimeDurability::Disabled => Err(SessionStoreError::Invalid(
            StoreInvalidReason::DisabledDurabilityRequiresMemoryStore,
        )),
    }
}
fn decode_durability(v: i64) -> Result<RuntimeDurability, SessionStoreError> {
    match v {
        1 => Ok(RuntimeDurability::Required),
        2 => Ok(RuntimeDurability::BestEffort),
        _ => Err(SessionStoreError::corrupt()),
    }
}
fn checkpoint_kind_tag(k: RuntimeCheckpointKind) -> i64 {
    match k {
        RuntimeCheckpointKind::Quiescent => 1,
        RuntimeCheckpointKind::Working => 2,
    }
}

fn transition_tag(transition: BranchTransitionMode) -> i64 {
    match transition {
        BranchTransitionMode::OrdinaryFork => 1,
        BranchTransitionMode::Rebind => 2,
    }
}

fn decode_transition(value: i64) -> Result<BranchTransitionMode, SessionStoreError> {
    match value {
        1 => Ok(BranchTransitionMode::OrdinaryFork),
        2 => Ok(BranchTransitionMode::Rebind),
        _ => Err(SessionStoreError::UnsupportedCodec),
    }
}

fn decode_checkpoint_kind(v: i64) -> Result<RuntimeCheckpointKind, SessionStoreError> {
    match v {
        1 => Ok(RuntimeCheckpointKind::Quiescent),
        2 => Ok(RuntimeCheckpointKind::Working),
        _ => Err(SessionStoreError::corrupt()),
    }
}
fn decode_address(k: i64, c: i64, d: Vec<u8>) -> Result<RuntimeObjectAddress, SessionStoreError> {
    let kind = u8::try_from(k)
        .ok()
        .and_then(|v| RuntimeObjectKind::from_store_tag(v).ok())
        .ok_or(SessionStoreError::UnsupportedCodec)?;
    let codec = u32::try_from(c).map_err(|_| SessionStoreError::UnsupportedCodec)?;
    let digest = Digest::from_sha256_bytes(DigestDomain::Blob, codec, array32(d)?)
        .map_err(|_| SessionStoreError::UnsupportedCodec)?;
    RuntimeObjectAddress::from_store_parts(kind, codec, digest).map_err(bundle_error)
}
fn decode_digest(domain: DigestDomain, v: i64, b: Vec<u8>) -> Result<Digest, SessionStoreError> {
    Digest::from_sha256_bytes(
        domain,
        u32::try_from(v).map_err(|_| SessionStoreError::UnsupportedCodec)?,
        array32(b)?,
    )
    .map_err(|_| SessionStoreError::UnsupportedCodec)
}
fn encode_address(out: &mut Vec<u8>, a: RuntimeObjectAddress) {
    out.push(kind_tag(a.kind()));
    out.extend_from_slice(&a.codec_revision().to_be_bytes());
    out.extend_from_slice(a.digest().bytes());
}
fn kind_tag(k: RuntimeObjectKind) -> u8 {
    k as u8
}
// The deadline blob is domain epoch (16), scope (32), then seconds (8).
// Its nanoseconds live in the companion integer column. UTC records use the
// independent timestamp codec below and never pass through this decoder.
fn lease_time(bytes: &[u8], nanoseconds: i64) -> Result<LeaseInstant, SessionStoreError> {
    let bytes: &[u8; 56] = bytes.try_into().map_err(|_| SessionStoreError::corrupt())?;
    LeaseInstant::new(
        decode_domain(&bytes[..48])?,
        read_u64(&bytes[48..])?,
        u32::try_from(nanoseconds).map_err(|_| SessionStoreError::corrupt())?,
    )
    .map_err(|_| SessionStoreError::corrupt())
}

fn lease_time_bytes(value: LeaseInstant) -> Vec<u8> {
    let mut bytes = domain_bytes(value.domain());
    bytes.extend_from_slice(&value.seconds().to_be_bytes());
    bytes
}

fn lease_instant_bytes(value: LeaseInstant) -> Vec<u8> {
    framed(
        b"lease-instant",
        &[
            domain_bytes(value.domain()),
            value.seconds().to_be_bytes().to_vec(),
            value.nanoseconds().to_be_bytes().to_vec(),
        ],
    )
}

fn timestamp(s: &[u8], ns: i64) -> Result<Timestamp, SessionStoreError> {
    Timestamp::new(
        read_u64(s)?,
        u32::try_from(ns).map_err(|_| SessionStoreError::corrupt())?,
    )
    .map_err(|_| SessionStoreError::corrupt())
}
fn u64b(v: u64) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}
fn read_u64(v: &[u8]) -> Result<u64, SessionStoreError> {
    <[u8; 8]>::try_from(v)
        .map(u64::from_be_bytes)
        .map_err(|_| SessionStoreError::corrupt())
}
fn nz(v: &[u8]) -> Result<NonZeroU64, SessionStoreError> {
    NonZeroU64::new(read_u64(v)?).ok_or(SessionStoreError::corrupt())
}
fn array32(v: Vec<u8>) -> Result<[u8; 32], SessionStoreError> {
    v.try_into().map_err(|_| SessionStoreError::corrupt())
}
fn workflow_id_bytes(value: SessionWorkflowId) -> Vec<u8> {
    value.as_bytes().to_vec()
}
fn decode_workflow_id(value: &[u8]) -> Result<SessionWorkflowId, SessionStoreError> {
    SessionWorkflowId::new(<[u8; 32]>::try_from(value).map_err(|_| SessionStoreError::corrupt())?)
        .map_err(|_| SessionStoreError::corrupt())
}
fn boolv(v: i64) -> Result<bool, SessionStoreError> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(SessionStoreError::corrupt()),
    }
}
fn parse<T: FromStr>(v: &str) -> Result<T, SessionStoreError> {
    v.parse().map_err(|_| SessionStoreError::corrupt())
}
fn id(v: impl fmt::Display) -> String {
    v.to_string()
}
fn nonnegative(v: i64) -> Result<u64, SessionStoreError> {
    u64::try_from(v).map_err(|_| SessionStoreError::corrupt())
}
fn i64n(v: u64) -> Result<i64, SessionStoreError> {
    i64::try_from(v)
        .map_err(|_| SessionStoreError::capacity(StoreCapacityBound::SqliteInteger, v, 0))
}
fn exactly_one(changed: usize) -> Result<(), SessionStoreError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(SessionStoreError::corrupt())
    }
}
fn enforce(name: StoreCapacityBound, value: u64, max: usize) -> Result<(), SessionStoreError> {
    let max = u64n(max);
    if value > max {
        Err(SessionStoreError::capacity(name, value, max))
    } else {
        Ok(())
    }
}
/// Holds an aggregate bound to `admission`'s share of `limit`.
fn admit(
    bound: StoreCapacityBound,
    attempted: u64,
    limit: usize,
    admission: StoreAdmission,
) -> Result<(), SessionStoreError> {
    let limit = u64n(limit);
    if attempted > admission.threshold(limit) {
        Err(SessionStoreError::admission_capacity(
            bound, attempted, limit, admission,
        ))
    } else {
        Ok(())
    }
}
fn usage(c: &Connection, limits: MemoryStoreLimits) -> Result<StoreUsage, SessionStoreError> {
    let stats = read_stats(c)?;
    Ok(StoreUsage::new(
        (stats.checkpoints, u64n(limits.max_checkpoints().get())),
        (stats.objects, u64n(limits.max_objects().get())),
        (stats.object_bytes, u64n(limits.max_object_bytes().get())),
    ))
}
/// Allocated database bytes and reusable free-list bytes.
fn file_usage(c: &Connection) -> Result<SqliteFileUsage, SessionStoreError> {
    let pragma = |name: &str| {
        c.pragma_query_value(None, name, |r| r.get::<_, i64>(0))
            .map_err(map_sqlite)
            .and_then(nonnegative)
    };
    let page_size = pragma("page_size")?;
    Ok(SqliteFileUsage {
        file_bytes: pragma("page_count")?.saturating_mul(page_size),
        free_bytes: pragma("freelist_count")?.saturating_mul(page_size),
    })
}
fn u64n(v: usize) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}
const fn nonzero(v: usize) -> NonZeroUsize {
    match NonZeroUsize::new(v) {
        Some(v) => v,
        None => panic!("nonzero"),
    }
}
fn create_receipt_integrity(receipt: &CreateReceipt) -> Result<Vec<u8>, SessionStoreError> {
    let operation = receipt.operation();
    receipt_integrity(
        b"create",
        &[
            session_ref_bytes(operation.session()),
            timestamp_bytes(operation.created_at()),
            text_bytes(operation.owner()),
            operation.lease_seconds().get().to_be_bytes().to_vec(),
            checkpoint_ref_bytes(operation.initial()),
            operation.checkpoint_commit_identity().identity_bytes(),
            session_head_bytes(receipt.session())?,
            branch_head_bytes(receipt.branch()),
            lease_bytes(receipt.lease()),
            receipt.feed_seq().get().to_be_bytes().to_vec(),
        ],
    )
}
fn commit_receipt_integrity(committed: &CommittedCheckpoint) -> Result<Vec<u8>, SessionStoreError> {
    let operation = committed.operation();
    let receipt = committed.receipt();
    receipt_integrity(
        b"commit",
        &[
            lease_bytes(operation.lease()),
            branch_head_bytes(operation.expected()),
            checkpoint_ref_bytes(operation.checkpoint()),
            operation.checkpoint_commit_identity().identity_bytes(),
            checkpoint_ref_bytes(receipt.checkpoint()),
            branch_head_bytes(receipt.previous()),
            branch_head_bytes(receipt.head()),
            receipt.feed_seq().get().to_be_bytes().to_vec(),
        ],
    )
}
fn fork_receipt_integrity(receipt: &ForkReceipt) -> Result<Vec<u8>, SessionStoreError> {
    let operation = receipt.operation();
    receipt_integrity(
        b"fork-v4",
        &[
            operation.workflow_id().as_bytes().to_vec(),
            operation.semantic_request().identity_bytes(),
            operation
                .expected_session_revision()
                .get()
                .to_be_bytes()
                .to_vec(),
            branch_head_bytes(operation.source()),
            session_ref_bytes(operation.target()),
            transition_tag(operation.transition())
                .to_be_bytes()
                .to_vec(),
            checkpoint_ref_bytes(operation.checkpoint()),
            operation.checkpoint_commit_identity().identity_bytes(),
            session_head_bytes(receipt.previous_session())?,
            session_head_bytes(receipt.session())?,
            branch_head_bytes(receipt.source()),
            branch_head_bytes(receipt.target()),
            lease_bytes(receipt.lease()),
            receipt.feed_seq().get().to_be_bytes().to_vec(),
        ],
    )
}
fn archive_receipt_integrity(receipt: ArchiveReceipt) -> Result<Vec<u8>, SessionStoreError> {
    receipt_integrity(
        b"archive",
        &[
            text_bytes(receipt.request().session_id()),
            receipt
                .request()
                .expected_revision()
                .get()
                .to_be_bytes()
                .to_vec(),
            session_head_bytes(receipt.previous())?,
            session_head_bytes(receipt.session())?,
        ],
    )
}
fn delete_receipt_integrity(receipt: DeleteReceipt) -> Result<Vec<u8>, SessionStoreError> {
    receipt_integrity(
        b"delete-v1",
        &[
            text_bytes(receipt.request().session_id()),
            receipt
                .request()
                .expected_revision()
                .get()
                .to_be_bytes()
                .to_vec(),
            session_head_bytes(receipt.archived())?,
            receipt.reclaimed_checkpoints().to_be_bytes().to_vec(),
        ],
    )
}
fn receipt_integrity(label: &[u8], fields: &[Vec<u8>]) -> Result<Vec<u8>, SessionStoreError> {
    let encoded = framed(label, fields);
    Digest::sha256(DigestDomain::Blob, 1, &[&encoded])
        .map(|digest| digest.bytes().to_vec())
        .map_err(|_| SessionStoreError::corrupt())
}
fn framed(label: &[u8], fields: &[Vec<u8>]) -> Vec<u8> {
    let mut encoded = Vec::new();
    encode_field(&mut encoded, label);
    for field in fields {
        encode_field(&mut encoded, field);
    }
    encoded
}
fn encode_field(encoded: &mut Vec<u8>, value: &[u8]) {
    encoded.extend_from_slice(&u64n(value.len()).to_be_bytes());
    encoded.extend_from_slice(value);
}
fn session_ref_bytes(value: SessionRef) -> Vec<u8> {
    framed(
        b"session-ref",
        &[
            text_bytes(value.session_id()),
            text_bytes(value.branch_id()),
        ],
    )
}
fn checkpoint_ref_bytes(value: CheckpointObjectRef) -> Vec<u8> {
    framed(
        b"checkpoint-ref",
        &[
            text_bytes(value.checkpoint_id()),
            address_bytes(value.root()),
            value.integrity().identity_bytes(),
        ],
    )
}
fn branch_head_bytes(value: BranchHead) -> Vec<u8> {
    framed(
        b"branch-head",
        &[
            session_ref_bytes(value.session()),
            value.revision().get().to_be_bytes().to_vec(),
            checkpoint_ref_bytes(value.checkpoint()),
        ],
    )
}
fn session_head_bytes(value: SessionHead) -> Result<Vec<u8>, SessionStoreError> {
    Ok(framed(
        b"session-head",
        &[
            text_bytes(value.session_id()),
            text_bytes(value.initial_branch()),
            value.revision().get().to_be_bytes().to_vec(),
            timestamp_bytes(value.created_at()),
            vec![durability_tag(value.durability())?],
            vec![u8::from(value.is_archived())],
        ],
    ))
}
fn lease_bytes(value: &BranchLease) -> Vec<u8> {
    framed(
        b"branch-lease",
        &[
            session_ref_bytes(value.session()),
            text_bytes(value.owner()),
            value.fence().get().to_be_bytes().to_vec(),
            lease_instant_bytes(value.expires_at()),
            value.lease_seconds().get().to_be_bytes().to_vec(),
        ],
    )
}
fn address_bytes(value: RuntimeObjectAddress) -> Vec<u8> {
    framed(
        b"object-address",
        &[
            vec![kind_tag(value.kind())],
            value.codec_revision().to_be_bytes().to_vec(),
            value.digest().identity_bytes(),
        ],
    )
}
fn timestamp_bytes(value: Timestamp) -> Vec<u8> {
    framed(
        b"timestamp",
        &[
            value.seconds().to_be_bytes().to_vec(),
            value.nanoseconds().to_be_bytes().to_vec(),
        ],
    )
}
fn text_bytes(value: impl fmt::Display) -> Vec<u8> {
    value.to_string().into_bytes()
}
#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned bundle failure"
)]
fn bundle_error(e: CheckpointBundleError) -> SessionStoreError {
    if e == CheckpointBundleError::UnsupportedRevision {
        SessionStoreError::UnsupportedCodec
    } else {
        SessionStoreError::corrupt()
    }
}
fn map_sqlite(error: rusqlite::Error) -> SessionStoreError {
    // The public error stays content-free. Keep the SQLite code in the local
    // diagnostic so an immediate FULL is distinguishable from a busy wait.
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        tracing::warn!(
            target: "plakous_store::sqlite_operation",
            sqlite_code = sqlite_failure_category(failure.code),
            sqlite_extended_code = failure.extended_code,
            "SQLite store operation failed"
        );
    } else {
        tracing::warn!(
            target: "plakous_store::sqlite_operation",
            error_category = rusqlite_error_category(&error),
            "SQLite store operation failed"
        );
    }
    let mapped = match &error {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::DiskFull => {
            SessionStoreError::StorageFull
        }
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) =>
        {
            SessionStoreError::BackendBusy
        }
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) =>
        {
            SessionStoreError::corrupt()
        }
        // Every lookup that may find nothing reads through `optional()`; a
        // missing row elsewhere is a store invariant breach, not an outage.
        rusqlite::Error::QueryReturnedNoRows
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::FromSqlConversionFailure(..) => SessionStoreError::corrupt(),
        _ => SessionStoreError::BackendUnavailable,
    };
    drop(error);
    mapped
}

/// A content-free category for errors raised by rusqlite rather than SQLite.
const fn rusqlite_error_category(error: &rusqlite::Error) -> &'static str {
    match error {
        rusqlite::Error::QueryReturnedNoRows => "no_rows",
        rusqlite::Error::InvalidQuery
        | rusqlite::Error::InvalidParameterName(_)
        | rusqlite::Error::InvalidParameterCount(..)
        | rusqlite::Error::InvalidColumnIndex(_)
        | rusqlite::Error::InvalidColumnName(_)
        | rusqlite::Error::ExecuteReturnedResults
        | rusqlite::Error::QueryReturnedMoreThanOneRow
        | rusqlite::Error::MultipleStatement => "invalid_query",
        rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::FromSqlConversionFailure(..) => "invalid_value",
        _ => "other_rusqlite",
    }
}

const fn sqlite_failure_category(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::DiskFull => "disk_full",
        ErrorCode::DatabaseBusy => "database_busy",
        ErrorCode::DatabaseLocked => "database_locked",
        ErrorCode::SystemIoFailure => "io_failure",
        ErrorCode::CannotOpen => "cannot_open",
        ErrorCode::ReadOnly => "read_only",
        ErrorCode::DatabaseCorrupt => "database_corrupt",
        ErrorCode::NotADatabase => "not_a_database",
        _ => "other_sqlite_failure",
    }
}
fn map_io(error: std::io::Error) -> SessionStoreError {
    let full = error.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error());
    drop(error);
    if full {
        SessionStoreError::StorageFull
    } else {
        SessionStoreError::BackendUnavailable
    }
}
fn prepare_private_file(path: &Path) -> Result<(), SessionStoreError> {
    match path.symlink_metadata() {
        Ok(_) => {
            let mut existing = OpenOptions::new();
            existing.read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                existing.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
            }
            let file = existing.open(path).map_err(map_io)?;
            return prepare_opened_private_file(&file);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_io(error)),
    }
    let mut o = OpenOptions::new();
    // `create_new` makes creation atomic and refuses a concurrently inserted
    // symlink. Existing files are never chmod'd by path: SQLite's subsequent
    // authoritative open uses SQLITE_OPEN_NOFOLLOW and verifies the schema.
    o.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    let file = match o.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new();
            existing.read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                existing.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
            }
            let file = existing.open(path).map_err(map_io)?;
            return prepare_opened_private_file(&file);
        }
        Err(error) => return Err(map_io(error)),
    };
    prepare_opened_private_file(&file)
}

fn prepare_opened_private_file(file: &File) -> Result<(), SessionStoreError> {
    let metadata = file.metadata().map_err(map_io)?;
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::BackendUnavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(map_io)?;
    }
    let metadata = file.metadata().map_err(map_io)?;
    require_private_regular_file(&metadata)
}

fn require_private_regular_file(metadata: &fs::Metadata) -> Result<(), SessionStoreError> {
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::BackendUnavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(SessionStoreError::BackendUnavailable);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated SQLite fixtures")]
mod persistence_shape_tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn deadline_codec_preserves_domain_and_refuses_old_or_malformed_bytes() {
        let domain = LeaseDomain::new([0; 16], [0; 32]);
        let deadline = LeaseInstant::new(domain, u64::MAX, 999_999_999).unwrap();
        let bytes = lease_time_bytes(deadline);
        assert_eq!(lease_time(&bytes, 999_999_999).unwrap(), deadline);
        assert_eq!(lease_time(&u64b(50), 0), Err(SessionStoreError::corrupt()));
        assert_eq!(lease_time(&bytes, -1), Err(SessionStoreError::corrupt()));
        assert_eq!(
            lease_time(&bytes, 1_000_000_000),
            Err(SessionStoreError::corrupt())
        );
        let foreign =
            LeaseInstant::new(LeaseDomain::new([1; 16], [0; 32]), u64::MAX, 999_999_999).unwrap();
        assert_ne!(lease_instant_bytes(deadline), lease_instant_bytes(foreign));
        let foreign =
            LeaseInstant::new(LeaseDomain::new([0; 16], [1; 32]), u64::MAX, 999_999_999).unwrap();
        assert_ne!(lease_instant_bytes(deadline), lease_instant_bytes(foreign));
    }

    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "plakous-store-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let database = root.join("runtime.sqlite3");
        (root, database)
    }

    #[test]
    fn current_shape_is_explicitly_non_authoritative() {
        let (root, database) = fixture("current-shape");
        drop(SqliteSessionStore::open(&database).unwrap());
        let connection = Connection::open(&database).unwrap();
        connection
            .execute("UPDATE store_meta SET sessions=1 WHERE id=1", [])
            .unwrap();
        drop(connection);

        assert_eq!(
            SqliteSessionStore::classify_persistence(&database).unwrap(),
            SqlitePersistenceShape::Current
        );
        assert!(matches!(
            SqliteSessionStore::open(&database),
            Err(SessionStoreError::Corrupt(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
const CRASH_CUT_STAGE_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_CUT";
#[cfg(test)]
const CRASH_CUT_MARKER_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_MARKER";

#[cfg(test)]
fn install_test_commit_hook(connection: &Connection) {
    if test_crash_cut_requested("pre-commit-hook") {
        let _ = connection.commit_hook(Some(|| {
            test_crash_cut("pre-commit-hook");
            false
        }));
    }
}

#[cfg(test)]
fn test_crash_cut_requested(stage: &str) -> bool {
    std::env::var(CRASH_CUT_STAGE_ENV).is_ok_and(|requested| requested == stage)
}

#[cfg(test)]
fn test_crash_signal(stage: &str) {
    if test_crash_cut_requested(stage)
        && let Some(marker) = std::env::var_os(CRASH_CUT_MARKER_ENV)
    {
        let _ = fs::write(marker, stage.as_bytes());
    }
}

#[cfg(test)]
fn test_crash_cut(stage: &str) {
    if !test_crash_cut_requested(stage) {
        return;
    }
    if let Some(marker) = std::env::var_os(CRASH_CUT_MARKER_ENV) {
        let _ = fs::write(marker, stage.as_bytes());
    }
    loop {
        thread::park();
    }
}

#[cfg(all(test, unix))]
use crate::integrity_fixture as crash_fixture;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "crash harness fixtures fail at exact lines"
)]
mod tests {
    use std::error::Error;
    use std::fs;
    #[cfg(unix)]
    use std::num::NonZeroU64;

    #[test]
    fn removed_branch_transition_tags_are_unsupported() {
        for tag in [3, 4] {
            assert_eq!(
                super::decode_transition(tag).err(),
                Some(SessionStoreError::UnsupportedCodec)
            );
        }
    }

    #[test]
    fn missing_publication_base_or_root_leaves_sqlite_state_unchanged() -> TestResult {
        use plakous_agent::internal::StoreIntegrityReason;
        for reason in [
            StoreIntegrityReason::MissingBase,
            StoreIntegrityReason::MissingRoot,
        ] {
            let (base, next) = fixture::integrity_publication_pair()?;
            let mut connection = Connection::open_in_memory()?;
            connection.execute_batch(super::SCHEMA)?;
            {
                let transaction = connection.transaction()?;
                super::insert_candidate(
                    &transaction,
                    &super::candidate(&base)?,
                    super::MemoryStoreLimits::default(),
                    plakous_agent::internal::StoreAdmission::Ordinary,
                )?;
                transaction.commit()?;
            }
            let before = super::read_stats(&connection)?;
            let mut candidate = super::candidate(&next)?;
            if reason == StoreIntegrityReason::MissingBase {
                candidate.base = Some(candidate.reference.root());
            } else {
                candidate
                    .objects
                    .retain(|object| object.address() != candidate.reference.root());
            }
            {
                let transaction = connection.transaction()?;
                assert_eq!(
                    super::insert_candidate(
                        &transaction,
                        &candidate,
                        super::MemoryStoreLimits::default(),
                        plakous_agent::internal::StoreAdmission::Ordinary
                    )
                    .err(),
                    Some(SessionStoreError::Corrupt(reason)),
                );
                transaction.commit()?;
            }
            assert_eq!(super::read_stats(&connection)?, before);
            assert_eq!(
                super::checkpoint_row(&connection, base.checkpoint_id())?
                    .ok_or("base checkpoint")?
                    .reference,
                super::bundle_ref(&base)
            );
            assert!(super::checkpoint_row(&connection, next.checkpoint_id())?.is_none());
        }
        Ok(())
    }

    /// An object address from another format epoch never decodes: the store
    /// reports it as an unsupported codec, which the facade surfaces as an
    /// incompatible build, not as corruption.
    #[test]
    fn an_object_from_another_format_epoch_is_an_unsupported_codec() {
        let kind = i64::from(super::kind_tag(super::RuntimeObjectKind::CheckpointRoot));
        let epoch = i64::from(plakous_agent::internal::RUNTIME_FORMAT_EPOCH);
        for other in [epoch - 1, epoch + 1] {
            assert_eq!(
                super::decode_address(kind, other, vec![0; 32]).err(),
                Some(SessionStoreError::UnsupportedCodec),
                "epoch {other}"
            );
        }
    }

    /// The object table admits exactly the current format epoch. The DDL
    /// spells the epoch as a literal, so a bump that forgets it makes every
    /// fresh store refuse its first object as "storage unavailable".
    #[test]
    fn the_object_table_admits_the_current_format_epoch() {
        let epoch = plakous_agent::internal::RUNTIME_FORMAT_EPOCH;
        assert!(
            super::SCHEMA.contains(&format!("CHECK(codec={epoch})")),
            "runtime_objects.codec CHECK disagrees with RUNTIME_FORMAT_EPOCH {epoch}"
        );
    }

    /// Release cost is bounded by construction, not by tuning: the lease
    /// release is one transaction (one WAL fsync under `synchronous=FULL`),
    /// `flush` is one passive checkpoint and `shutdown` one truncating
    /// checkpoint. Prune schedules separate background housekeeping; release
    /// itself never checkpoints (preserves `synchronous=FULL`).
    #[test]
    fn release_flush_and_shutdown_perform_a_bounded_number_of_fsyncs() {
        let source = include_str!("sqlite.rs")
            .split("\nmod tests {")
            .next()
            .expect("production half of the module");
        let release = source
            .split("\nfn release(b: &mut Backend, l: &BranchLease)")
            .nth(1)
            .and_then(|rest| rest.split("\nfn write_active(").next())
            .expect("release body");
        assert_eq!(
            release.matches("transaction_with_behavior(").count(),
            1,
            "release opens exactly one transaction"
        );
        assert_eq!(
            release.matches("tx.commit()").count(),
            2,
            "release commits once on the idempotent path and once on the write path"
        );
        assert!(
            !release.contains("checkpoint_wal("),
            "release never checkpoints; flush and shutdown own that"
        );
        let calls = source.matches("checkpoint_wal(&b.connection, ").count();
        assert_eq!(
            calls, 2,
            "two foreground checkpoint call sites: flush (passive), shutdown (truncate)"
        );
        assert!(source.contains("let _ = checkpoint_wal(&self.connection, false);"));
        assert!(source.contains("checkpoint_wal(&b.connection, false)?;"));
        assert!(source.contains("checkpoint_wal(&b.connection, true)?;"));
        assert!(
            source.contains("PRAGMA synchronous=FULL;"),
            "the durability pragma stays in force"
        );
    }

    #[cfg(unix)]
    use std::path::Path;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;

    use plakous_agent::internal::{
        CheckpointId, CheckpointObjectBundle, CommitCheckpoint, CreateReceipt, CreateSession,
        LeaseClock, RuntimeCheckpoint, RuntimeDurability, SessionRef, SessionStore,
        SessionStoreError, Timestamp,
    };
    use plakous_model::{BranchId, RunId, SessionId};
    use rusqlite::Connection;
    #[cfg(unix)]
    use rusqlite::trace::{TraceEvent, TraceEventCodes};

    #[cfg(unix)]
    use plakous_agent::internal::{CommitReceipt, FeedChange, FeedPage, FeedSeq};
    #[cfg(unix)]
    use std::num::NonZeroUsize;

    #[cfg(unix)]
    use super::{CRASH_CUT_MARKER_ENV, CRASH_CUT_STAGE_ENV, SqliteStoreStats, verify_database};
    use super::{
        Job, SqliteSessionStore, StartupProgress, StartupStage, Worker, crash_fixture as fixture,
        map_sqlite, sqlite_failure_category, startup_error_category, wait_for_actor,
        wait_for_actor_or_join,
    };

    #[cfg(unix)]
    const CRASH_DB_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_DB";
    #[cfg(unix)]
    const CRASH_HISTORY_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_HISTORY";
    #[cfg(unix)]
    const CRASH_CHILD_TEST: &str = "sqlite::tests::sqlite_crash_cut_child";
    #[cfg(unix)]
    const CRASH_WAIT: Duration = Duration::from_secs(30);
    static NEXT_CRASH_DIRECTORY: AtomicU64 = AtomicU64::new(1);
    #[cfg(unix)]
    static STARTUP_QUERY_COUNT: AtomicU64 = AtomicU64::new(0);
    #[cfg(unix)]
    static STARTUP_QUERY_TEST: Mutex<()> = Mutex::new(());

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    #[cfg(unix)]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "rusqlite trace_v2 requires this exact function-pointer signature"
    )]
    fn count_startup_query(event: TraceEvent<'_>) {
        if matches!(event, TraceEvent::Stmt(..)) {
            STARTUP_QUERY_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct CrashDatabase {
        directory: PathBuf,
        path: PathBuf,
    }

    #[cfg(unix)]
    pub(super) struct KillOnDrop(Option<Child>);

    #[cfg(unix)]
    impl KillOnDrop {
        pub(super) fn new(child: Child) -> Self {
            Self(Some(child))
        }

        pub(super) fn child(&mut self) -> &mut Child {
            self.0.as_mut().expect("live crash child")
        }

        pub(super) fn sigkill(mut self) -> std::io::Result<std::process::ExitStatus> {
            let status = {
                let child = self.0.as_mut().expect("live crash child");
                child.kill()?;
                child.wait()
            };
            if status.is_ok() {
                let _ = self.0.take();
            }
            status
        }
    }

    #[cfg(unix)]
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl CrashDatabase {
        fn new() -> TestResult<Self> {
            for _ in 0..1_024 {
                let directory = std::env::temp_dir().join(format!(
                    "plakous-store-crash-{}-{}",
                    std::process::id(),
                    NEXT_CRASH_DIRECTORY.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&directory) {
                    Ok(()) => {
                        let path = directory.join("sessions.sqlite3");
                        return Ok(Self { directory, path });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a unique crash-test directory",
            )
            .into())
        }
    }

    impl Drop for CrashDatabase {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    struct CrashSeed {
        selector: SessionRef,
        checkpoint_id: CheckpointId,
        checkpoint: RuntimeCheckpoint,
        bundle: CheckpointObjectBundle,
        request: CreateSession,
    }

    #[cfg(unix)]
    struct CrashCommit {
        request: CommitCheckpoint,
        checkpoint: RuntimeCheckpoint,
    }

    fn crash_seed(base: u128) -> TestResult<CrashSeed> {
        let session_id = fixture::id::<SessionId>(base)?;
        let branch_id = fixture::id::<BranchId>(base + 1)?;
        let checkpoint_id = fixture::id::<CheckpointId>(base + 2)?;
        let owner = fixture::id::<RunId>(base + 3)?;
        let selector = SessionRef::new(session_id, branch_id);
        let checkpoint = fixture::checkpoint(
            fixture::topology(session_id, branch_id, checkpoint_id, None, 0, None, 1)?,
            1,
            RuntimeDurability::Required,
        )?;
        let bundle = fixture::bundle(&checkpoint, None)?;
        let request = CreateSession::new(
            selector,
            Timestamp::new(50, 0)?,
            owner,
            NonZeroU64::new(3_600).ok_or("nonzero lease")?,
            bundle.clone(),
        );
        Ok(CrashSeed {
            selector,
            checkpoint_id,
            checkpoint,
            bundle,
            request,
        })
    }

    #[tokio::test]
    async fn cold_start_query_count_is_constant_for_large_authoritative_history() -> TestResult {
        const COMMITS: u64 = 128;
        let database = CrashDatabase::new()?;
        let seed = crash_seed(81_000)?;
        let store = SqliteSessionStore::open(&database.path)?;
        let created = store.create(seed.request.clone()).await?;
        let mut head = created.branch();
        let mut parent = seed.checkpoint_id;
        let mut prior = seed.bundle.clone();
        for revision in 1..=COMMITS {
            let checkpoint_id = fixture::id::<CheckpointId>(81_100 + u128::from(revision))?;
            let checkpoint = fixture::checkpoint(
                fixture::topology(
                    seed.selector.session_id(),
                    seed.selector.branch_id(),
                    checkpoint_id,
                    Some(parent),
                    revision,
                    None,
                    created.lease().fence().get(),
                )?,
                u32::try_from(revision + 1)?,
                RuntimeDurability::Required,
            )?;
            let bundle = fixture::bundle(&checkpoint, Some(&prior))?;
            head = store
                .commit(CommitCheckpoint::new(
                    created.lease().clone(),
                    head,
                    bundle.clone(),
                ))
                .await?
                .head();
            parent = checkpoint_id;
            prior = bundle;
        }
        store.shutdown().await?;
        drop(store);

        let _guard = STARTUP_QUERY_TEST
            .lock()
            .map_err(|_| "startup query lock")?;
        let connection = Connection::open(&database.path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;
             PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        STARTUP_QUERY_COUNT.store(0, Ordering::Relaxed);
        connection.trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_startup_query),
        );
        verify_database(&connection)?;
        connection.trace_v2(TraceEventCodes::empty(), None);
        let query_count = STARTUP_QUERY_COUNT.load(Ordering::Relaxed);
        assert!(
            query_count <= 32,
            "cold-start authority validation executed {query_count} statements for {COMMITS} commits"
        );
        assert_eq!(head.revision().get(), COMMITS);
        Ok(())
    }

    #[cfg(unix)]
    fn crash_commit(
        seed: &CrashSeed,
        created: &CreateReceipt,
        checkpoint_id: CheckpointId,
        history_len: u32,
    ) -> TestResult<CrashCommit> {
        let checkpoint = fixture::checkpoint(
            fixture::topology(
                seed.selector.session_id(),
                seed.selector.branch_id(),
                checkpoint_id,
                Some(seed.checkpoint_id),
                1,
                None,
                created.lease().fence().get(),
            )?,
            history_len,
            RuntimeDurability::Required,
        )?;
        let request = CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            fixture::bundle(&checkpoint, Some(&seed.bundle))?,
        );
        Ok(CrashCommit {
            request,
            checkpoint,
        })
    }

    #[cfg(unix)]
    pub(super) fn wait_for_marker(child: &mut Child, marker: &Path) -> TestResult {
        let deadline = Instant::now() + CRASH_WAIT;
        loop {
            if marker.is_file() {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                return Err(std::io::Error::other(format!(
                    "crash child exited before its fence: {status}"
                ))
                .into());
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "crash child did not reach its fence",
                )
                .into());
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    #[cfg(unix)]
    fn assert_old_stats(actual: SqliteStoreStats, baseline: SqliteStoreStats) {
        assert_eq!(actual, baseline);
    }

    #[cfg(unix)]
    fn assert_new_stats(
        actual: SqliteStoreStats,
        baseline: SqliteStoreStats,
        request: &CommitCheckpoint,
    ) {
        let added_objects = u64::try_from(request.checkpoint().supplied_objects().len())
            .expect("fixture object count fits u64");
        let added_bytes = u64::try_from(
            request
                .checkpoint()
                .supplied_objects()
                .iter()
                .map(plakous_agent::internal::RuntimeObject::retained_bytes)
                .sum::<usize>(),
        )
        .expect("fixture object bytes fit u64");
        assert_eq!(actual.sessions(), baseline.sessions());
        assert_eq!(actual.branches(), baseline.branches());
        assert_eq!(actual.checkpoints(), baseline.checkpoints() + 1);
        assert_eq!(actual.objects(), baseline.objects() + added_objects);
        assert_eq!(actual.object_bytes(), baseline.object_bytes() + added_bytes);
        assert_eq!(
            actual.semantic_mutations(),
            baseline.semantic_mutations() + 1
        );
    }

    /// The commit's record exists exactly when its commit does: the next
    /// sequence after the genesis, an `Advanced` record naming the commit,
    /// and the sequence its receipt returns.
    #[cfg(unix)]
    async fn assert_commit_record(
        store: &SqliteSessionStore,
        seed: &CrashSeed,
        checkpoint_id: CheckpointId,
        receipt: &CommitReceipt,
    ) -> TestResult {
        let session_id = seed.selector.session_id();
        assert_eq!(receipt.feed_seq(), FeedSeq::new(2));
        assert_eq!(store.feed_tail(session_id).await?.tail(), FeedSeq::new(2));
        let FeedPage::Records { records, .. } = store
            .read_feed(session_id, FeedSeq::new(1), NonZeroUsize::MIN)
            .await?
        else {
            return Err("the commit record fell behind retention".into());
        };
        assert_eq!(records.len(), 1);
        assert!(matches!(
            records[0].header().change,
            FeedChange::Advanced { checkpoint, .. } if checkpoint == checkpoint_id
        ));
        Ok(())
    }

    #[cfg(unix)]
    async fn assert_old_or_new_reopen(
        database: &CrashDatabase,
        seed: &CrashSeed,
        created: &CreateReceipt,
        commit: &CrashCommit,
        checkpoint_id: CheckpointId,
        baseline: SqliteStoreStats,
    ) -> TestResult<bool> {
        let reopened = SqliteSessionStore::open(&database.path)?;
        let head = reopened.load_branch(seed.selector).await?.head();
        let committed = if head.checkpoint().checkpoint_id() == checkpoint_id {
            assert_eq!(head.revision().get(), 1);
            let recovered = reopened.recover_commit(checkpoint_id).await?;
            assert_eq!(
                reopened.commit(commit.request.clone()).await?,
                recovered.receipt()
            );
            assert_eq!(recovered.operation().lease(), commit.request.lease());
            assert_eq!(recovered.operation().expected(), commit.request.expected());
            assert_eq!(
                recovered.operation().checkpoint_commit_identity(),
                commit.checkpoint.checkpoint_commit_identity()
            );
            let loaded = reopened.load_checkpoint(checkpoint_id).await?;
            assert_eq!(loaded.reference(), recovered.receipt().checkpoint());
            assert_eq!(loaded.checkpoint().as_ref(), &commit.checkpoint);
            assert_new_stats(reopened.stats().await?, baseline, &commit.request);
            assert_commit_record(&reopened, seed, checkpoint_id, &recovered.receipt()).await?;
            true
        } else {
            assert_eq!(head, created.branch());
            assert_eq!(
                reopened.recover_commit(checkpoint_id).await,
                Err(SessionStoreError::NotFound)
            );
            assert_eq!(
                reopened.load_checkpoint(checkpoint_id).await,
                Err(SessionStoreError::NotFound)
            );
            assert_eq!(
                reopened
                    .load_checkpoint(seed.checkpoint_id)
                    .await?
                    .checkpoint()
                    .as_ref(),
                &seed.checkpoint
            );
            assert_old_stats(reopened.stats().await?, baseline);
            assert_eq!(
                reopened.feed_tail(seed.selector.session_id()).await?.tail(),
                created.feed_seq(),
                "a cut before commit leaves no record"
            );
            let receipt = reopened.commit(commit.request.clone()).await?;
            assert_eq!(reopened.commit(commit.request.clone()).await?, receipt);
            let recovered = reopened.recover_commit(checkpoint_id).await?;
            assert_eq!(
                recovered.operation().checkpoint_commit_identity(),
                commit.checkpoint.checkpoint_commit_identity()
            );
            assert_eq!(recovered.receipt(), receipt);
            assert_eq!(
                reopened.load_branch(seed.selector).await?.head(),
                receipt.head()
            );
            let loaded = reopened.load_checkpoint(checkpoint_id).await?;
            assert_eq!(loaded.checkpoint().as_ref(), &commit.checkpoint);
            assert_new_stats(reopened.stats().await?, baseline, &commit.request);
            assert_commit_record(&reopened, seed, checkpoint_id, &receipt).await?;
            false
        };
        Ok(committed)
    }

    #[test]
    fn actor_readiness_timeout_keeps_its_stage_after_late_completion() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let progress = StartupProgress(Mutex::new(StartupStage::Schema));
        let failure =
            wait_for_actor(&receiver, Duration::ZERO, &progress).expect_err("readiness expires");
        progress.set(StartupStage::Ready);
        assert_eq!(failure.stage, StartupStage::Schema);
        assert_eq!(failure.category, "readiness_timeout");
        assert_eq!(failure.report(), SessionStoreError::BackendUnavailable);
    }

    #[test]
    fn actor_readiness_disconnect_is_distinct_from_timeout() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);
        let progress = StartupProgress(Mutex::new(StartupStage::Connection));
        let failure = wait_for_actor(&receiver, Duration::ZERO, &progress)
            .expect_err("readiness disconnected");
        assert_eq!(failure.stage, StartupStage::Connection);
        assert_eq!(failure.category, "readiness_disconnected");
        assert_eq!(failure.report(), SessionStoreError::BackendUnavailable);
    }

    #[test]
    fn actor_readiness_backend_failure_preserves_original_category_and_error() {
        for error in [
            SessionStoreError::BackendBusy,
            SessionStoreError::UnsupportedSchema,
        ] {
            let (sender, receiver) = mpsc::sync_channel(1);
            sender.send(Err(error)).expect("backend failure");
            let progress = StartupProgress(Mutex::new(StartupStage::Admission));
            let failure = wait_for_actor(&receiver, Duration::ZERO, &progress)
                .expect_err("backend refused open");
            assert_eq!(failure.stage, StartupStage::Admission);
            assert_eq!(failure.category, "backend");
            assert_eq!(failure.report(), error);
        }
        assert_eq!(
            startup_error_category(SessionStoreError::BackendBusy),
            "backend_busy"
        );
        assert_eq!(
            startup_error_category(SessionStoreError::BackendUnavailable),
            "backend_unavailable"
        );
    }

    #[test]
    fn sqlite_full_and_busy_keep_distinct_safe_diagnostics() {
        assert_eq!(
            super::map_io(std::io::Error::from_raw_os_error(
                rustix::io::Errno::NOSPC.raw_os_error()
            )),
            SessionStoreError::StorageFull
        );
        let connection = Connection::open_in_memory().expect("memory SQLite");
        connection
            .execute_batch("CREATE TABLE full_test(value BLOB);")
            .expect("table");
        let pages: i64 = connection
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .expect("pages");
        connection
            .pragma_update(None, "max_page_count", pages)
            .expect("fixed page budget");
        let error = connection
            .execute("INSERT INTO full_test VALUES(zeroblob(1048576))", [])
            .expect_err("the physical page budget is full");
        assert_eq!(map_sqlite(error), SessionStoreError::StorageFull);
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM full_test", [], |row| row
                    .get::<_, i64>(0))
                .expect("count"),
            0
        );
        for (code, category, expected) in [
            (
                rusqlite::ffi::SQLITE_FULL,
                "disk_full",
                SessionStoreError::StorageFull,
            ),
            (
                rusqlite::ffi::SQLITE_BUSY,
                "database_busy",
                SessionStoreError::BackendBusy,
            ),
        ] {
            let sqlite = rusqlite::ffi::Error::new(code);
            assert_eq!(sqlite_failure_category(sqlite.code), category);
            let error = rusqlite::Error::SqliteFailure(
                sqlite,
                Some("private database path and SQL".to_owned()),
            );
            assert_eq!(map_sqlite(error), expected);
        }
    }

    #[test]
    fn actor_startup_timeout_is_reported_only_after_worker_exit() {
        let (job_sender, job_receiver) = mpsc::sync_channel::<Job>(1);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (join_boundary_sender, join_boundary_receiver) = mpsc::sync_channel(1);
        let (exit_sender, exit_receiver) = mpsc::sync_channel(1);
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let worker = std::thread::spawn(move || {
            assert!(job_receiver.recv().is_err());
            join_boundary_sender.send(()).expect("join boundary");
            exit_receiver.recv().expect("worker exit release");
            assert!(
                ready_sender.send(Ok(())).is_err(),
                "late readiness stays refused"
            );
            worker_exited.store(true, Ordering::SeqCst);
        });
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || {
            let progress = StartupProgress(Mutex::new(StartupStage::Schema));
            let result = wait_for_actor_or_join(
                ready_receiver,
                job_sender,
                worker,
                Duration::ZERO,
                &progress,
            );
            result_sender
                .send(result.map(|_| ()))
                .expect("startup result");
        });

        join_boundary_receiver.recv().expect("actor entered join");
        assert!(!exited.load(Ordering::SeqCst));
        assert!(matches!(
            result_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        exit_sender.send(()).expect("release worker exit");
        assert_eq!(
            result_receiver.recv().expect("startup result after exit"),
            Err(SessionStoreError::BackendUnavailable)
        );
        assert!(exited.load(Ordering::SeqCst));
        waiter.join().expect("waiter exits");
    }

    #[tokio::test]
    async fn actor_dispatch_reports_full_and_failed_workers_without_waiting() {
        let (full_sender, full_receiver) = super::actor_queue(1);
        assert!(
            full_sender
                .send(super::Lane::Critical, Box::new(|_| {}))
                .is_ok()
        );
        let full = SqliteSessionStore {
            clock: Arc::new(super::SystemLeaseClock::new().expect("system lease clock")),
            domain: super::SystemLeaseClock::new()
                .expect("system lease clock")
                .now()
                .expect("clock reading")
                .domain(),
            sender: full_sender,
            usage_snapshot: Arc::new(Mutex::new(Err(SessionStoreError::BackendUnavailable))),
            worker: Arc::new(Worker {
                join: Mutex::new(None),
            }),
            #[cfg(feature = "perf-instrumentation")]
            trace: None,
        };
        assert_eq!(
            full.dispatch(
                "actor_dispatch_reports_full_and_failed_workers_without_waiting",
                |_| Ok::<_, SessionStoreError>(())
            )
            .await,
            Err(SessionStoreError::BackendBusy)
        );
        drop(full_receiver);

        let (failed_sender, failed_receiver) = super::actor_queue(1);
        drop(failed_receiver);
        let failed = SqliteSessionStore {
            clock: Arc::new(super::SystemLeaseClock::new().expect("system lease clock")),
            domain: super::SystemLeaseClock::new()
                .expect("system lease clock")
                .now()
                .expect("clock reading")
                .domain(),
            sender: failed_sender,
            usage_snapshot: Arc::new(Mutex::new(Err(SessionStoreError::BackendUnavailable))),
            worker: Arc::new(Worker {
                join: Mutex::new(None),
            }),
            #[cfg(feature = "perf-instrumentation")]
            trace: None,
        };
        assert_eq!(
            failed
                .dispatch(
                    "actor_dispatch_reports_full_and_failed_workers_without_waiting",
                    |_| Ok::<_, SessionStoreError>(())
                )
                .await,
            Err(SessionStoreError::BackendUnavailable)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sqlite_crash_cut_child() {
        let Some(path) = std::env::var_os(CRASH_DB_ENV).map(PathBuf::from) else {
            return;
        };
        let seed = crash_seed(50_000).expect("seed");
        let checkpoint_id = fixture::id::<CheckpointId>(50_010).expect("checkpoint id");
        let history_len = std::env::var(CRASH_HISTORY_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2);
        let store = SqliteSessionStore::open(path).expect("open child store");
        let created = store
            .recover_create(seed.request.clone())
            .await
            .expect("recover create");
        let commit =
            crash_commit(&seed, &created, checkpoint_id, history_len).expect("commit request");
        let result = store.commit(commit.request).await;
        panic!("configured crash cut did not stop the child: {result:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigkill_commit_cuts_reopen_as_exact_old_or_new_authority() -> TestResult {
        use std::os::unix::process::ExitStatusExt as _;

        for (stage, committed) in [
            ("before-commit", false),
            ("pre-commit-hook", false),
            ("post-commit-pre-reply", true),
        ] {
            let database = CrashDatabase::new()?;
            let seed = crash_seed(50_000)?;
            let checkpoint_id = fixture::id::<CheckpointId>(50_010)?;
            let store = SqliteSessionStore::open(&database.path)?;
            let created = store.create(seed.request.clone()).await?;
            let commit = crash_commit(&seed, &created, checkpoint_id, 2)?;
            let baseline = store.stats().await?;
            drop(store);

            let marker = database.directory.join(format!("{stage}.reached"));
            let child = Command::new(std::env::current_exe()?)
                .arg("--exact")
                .arg(CRASH_CHILD_TEST)
                .arg("--nocapture")
                .env(CRASH_DB_ENV, &database.path)
                .env(CRASH_CUT_STAGE_ENV, stage)
                .env(CRASH_CUT_MARKER_ENV, &marker)
                .env("RUST_TEST_THREADS", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let mut child = KillOnDrop::new(child);
            wait_for_marker(child.child(), &marker)?;
            let status = child.sigkill()?;
            assert_eq!(status.signal(), Some(9));

            assert_eq!(
                assert_old_or_new_reopen(
                    &database,
                    &seed,
                    &created,
                    &commit,
                    checkpoint_id,
                    baseline,
                )
                .await?,
                committed
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit() -> TestResult {
        use std::os::unix::process::ExitStatusExt as _;

        const HISTORY_ITEMS: u32 = 1_024;
        let delays = [
            Duration::ZERO,
            Duration::from_micros(50),
            Duration::from_micros(200),
            Duration::from_micros(500),
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(5),
            Duration::from_millis(10),
        ];
        for (attempt, delay) in delays.into_iter().enumerate() {
            let database = CrashDatabase::new()?;
            let seed = crash_seed(50_000)?;
            let checkpoint_id = fixture::id::<CheckpointId>(50_010)?;
            let store = SqliteSessionStore::open(&database.path)?;
            let created = store.create(seed.request.clone()).await?;
            let commit = crash_commit(&seed, &created, checkpoint_id, HISTORY_ITEMS)?;
            let baseline = store.stats().await?;
            drop(store);

            let marker = database
                .directory
                .join(format!("async-in-commit-{attempt}.reached"));
            let child = Command::new(std::env::current_exe()?)
                .arg("--exact")
                .arg(CRASH_CHILD_TEST)
                .arg("--nocapture")
                .env(CRASH_DB_ENV, &database.path)
                .env(CRASH_HISTORY_ENV, HISTORY_ITEMS.to_string())
                .env(CRASH_CUT_STAGE_ENV, "async-in-commit")
                .env(CRASH_CUT_MARKER_ENV, &marker)
                .env("RUST_TEST_THREADS", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let mut child = KillOnDrop::new(child);
            wait_for_marker(child.child(), &marker)?;
            std::thread::sleep(delay);
            let status = child.sigkill()?;
            assert_eq!(status.signal(), Some(9));

            assert_old_or_new_reopen(&database, &seed, &created, &commit, checkpoint_id, baseline)
                .await?;
        }
        Ok(())
    }

    #[cfg(unix)]
    const PRUNE_CRASH_DB_ENV: &str = "PLAKOUS_SQLITE_TEST_PRUNE_CRASH_DB";
    #[cfg(unix)]
    const PRUNE_CRASH_CHILD_TEST: &str = "sqlite::tests::sqlite_prune_crash_cut_child";
    #[cfg(unix)]
    const PRUNE_CRASH_COMMITS: u64 = 8;

    /// Creates the crash seed's session and commits `PRUNE_CRASH_COMMITS`
    /// successors, each with one more history item than its parent.
    #[cfg(unix)]
    async fn prune_crash_chain(
        store: &SqliteSessionStore,
    ) -> TestResult<(CreateReceipt, plakous_agent::internal::BranchHead)> {
        let seed = crash_seed(60_000)?;
        let created = store.create(seed.request.clone()).await?;
        let mut head = created.branch();
        let mut prior = seed.bundle.clone();
        let mut parent = seed.checkpoint_id;
        for revision in 1..=PRUNE_CRASH_COMMITS {
            let checkpoint_id = prune_crash_id(revision)?;
            let checkpoint = fixture::checkpoint(
                fixture::topology(
                    seed.selector.session_id(),
                    seed.selector.branch_id(),
                    checkpoint_id,
                    Some(parent),
                    revision,
                    None,
                    created.lease().fence().get(),
                )?,
                u32::try_from(revision)? + 1,
                RuntimeDurability::Required,
            )?;
            let publication = fixture::bundle(&checkpoint, Some(&prior))?;
            head = store
                .commit(CommitCheckpoint::new(
                    created.lease().clone(),
                    head,
                    publication.clone(),
                ))
                .await?
                .head();
            prior = publication;
            parent = checkpoint_id;
        }
        Ok((created, head))
    }

    #[cfg(unix)]
    fn prune_crash_id(revision: u64) -> TestResult<CheckpointId> {
        fixture::id::<CheckpointId>(60_100 + u128::from(revision))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sqlite_prune_crash_cut_child() {
        let Some(path) = std::env::var_os(PRUNE_CRASH_DB_ENV).map(PathBuf::from) else {
            return;
        };
        let store = SqliteSessionStore::open(path).expect("open child store");
        let (created, head) = prune_crash_chain(&store).await.expect("chain");
        let result = store
            .prune(plakous_agent::internal::PruneBranch::new(
                created.lease().clone(),
                head,
                std::collections::BTreeSet::new(),
                1,
            ))
            .await;
        panic!("configured crash cut did not stop the child: {result:?}");
    }

    /// A prune killed at any cut reopens, fully verified, as exactly the
    /// unpruned or the pruned store, never anything between.
    #[cfg(unix)]
    #[tokio::test]
    async fn sigkill_prune_cuts_reopen_as_exact_unpruned_or_pruned_history() -> TestResult {
        use std::os::unix::process::ExitStatusExt as _;

        for (stage, committed) in [
            ("prune-after-enqueue", false),
            ("prune-before-commit", false),
            ("prune-after-commit", true),
        ] {
            let database = CrashDatabase::new()?;
            let marker = database.directory.join(format!("{stage}.reached"));
            let child = Command::new(std::env::current_exe()?)
                .arg("--exact")
                .arg(PRUNE_CRASH_CHILD_TEST)
                .arg("--nocapture")
                .env(PRUNE_CRASH_DB_ENV, &database.path)
                .env(CRASH_CUT_STAGE_ENV, stage)
                .env(CRASH_CUT_MARKER_ENV, &marker)
                .env("RUST_TEST_THREADS", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let mut child = KillOnDrop::new(child);
            wait_for_marker(child.child(), &marker)?;
            let status = child.sigkill()?;
            assert_eq!(status.signal(), Some(9));

            drop(SqliteSessionStore::open_verified(&database.path)?);
            let reopened = SqliteSessionStore::open(&database.path)?;
            let seed = crash_seed(60_000)?;
            let head = reopened.load_branch(seed.selector).await?.head();
            assert_eq!(
                head.checkpoint().checkpoint_id(),
                prune_crash_id(PRUNE_CRASH_COMMITS)?
            );
            reopened.load_checkpoint(seed.checkpoint_id).await?;
            for revision in 1..=PRUNE_CRASH_COMMITS {
                let loaded = reopened
                    .load_checkpoint(prune_crash_id(revision)?)
                    .await
                    .map(|_| ());
                if committed && revision + 1 < PRUNE_CRASH_COMMITS {
                    assert_eq!(loaded, Err(SessionStoreError::Pruned), "{stage} {revision}");
                } else {
                    assert_eq!(loaded, Ok(()), "{stage} {revision}");
                }
            }
            let retained = reopened.stats().await?;
            let expected = if committed {
                3
            } else {
                PRUNE_CRASH_COMMITS + 1
            };
            assert_eq!(retained.checkpoints(), expected, "{stage}");
            reopened.shutdown().await?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    #[test]
    fn sigkill_commit_cuts_reopen_as_exact_old_or_new_authority() -> TestResult {
        eprintln!("skipped: Child::kill has SIGKILL semantics only on Unix");
        Ok(())
    }

    #[cfg(not(unix))]
    #[test]
    fn bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit() -> TestResult {
        eprintln!("skipped: Child::kill has SIGKILL semantics only on Unix");
        Ok(())
    }
}
