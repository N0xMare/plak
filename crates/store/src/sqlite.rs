//! Fresh `SQLite` representation of the semantic checkpoint store.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

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
    LeaseDomain, LeaseInstant, LineOrigin, LineStep, LoadedBranch, Page, PruneBranch, PruneReceipt,
    RecoverFork, RunId, RuntimeCheckpoint, RuntimeCheckpointKind, RuntimeDurability,
    RuntimeManifest, RuntimeObject, RuntimeObjectAddress, RuntimeObjectKind, SessionHead,
    SessionId, SessionQuery, SessionRef, SessionRevision, SessionStore, SessionStoreError,
    SessionSummary, SessionWorkflowId, StoreAdmission, StoreCapacityBound, StoreFuture,
    StoreInvalidReason, StoreUsage, StoredCheckpoint, Timestamp,
};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use super::lease_clock::SystemLeaseClock;

pub use crate::holder::{BranchHolder, branch_holder};

mod actor;
#[cfg(test)]
mod actor_tests;
#[cfg(test)]
mod admission_budget_tests;
mod authority;
mod checkpoint_cache;
mod codec;
mod error;
mod feed;
mod gc;
#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod lease_sync_tests;
mod lines;
mod objects;
mod open;
mod ops;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated SQLite fixtures")]
mod persistence_shape_tests;
mod reads;
mod retention;
pub use retention::SqliteSessionRetention;
mod rows;
mod schema;
#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "crash harness fixtures fail at exact lines"
)]
mod tests;

use super::MemoryStoreLimits;
use super::memory::{expired, expiry, require_lease_domain, sample_clock};
use actor::*;
use authority::*;
use checkpoint_cache::CheckpointCache;
use codec::*;
use error::*;
use objects::*;
use open::*;
use ops::{
    acquire, archive, commit, create, delete, fork, incremental_vacuum, load_delete, prune,
    recover_commit, recover_create, recover_fork, release, release_many, renew, revive,
};
use plakous_agent::internal::{StoreReclaimMode, StoreReclaimReport};
use reads::*;
use rows::*;
use schema::*;

/// The runtime store's schema version. Only this exact version opens.
pub const SQLITE_SCHEMA_VERSION: i64 = 27;
/// Reviewed `SQLite` release statically embedded by the workspace dependency.
pub const BUNDLED_SQLITE_VERSION: &str = "3.54.0";
/// Upstream source identity of [`BUNDLED_SQLITE_VERSION`].
pub const BUNDLED_SQLITE_SOURCE_ID: &str =
    "2026-10-09 15:46:58 be8d059e9a49089ab2dce5ed26dd87aaf598fdc5fbe0b107c0dd758464bcd5e3";
/// Security and concurrency options required from the reviewed bundled build.
pub const BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS: [&str; 4] = [
    "DEFAULT_FOREIGN_KEYS",
    "ENABLE_API_ARMOR",
    "THREADSAFE=1",
    "USE_URI",
];
const BACKGROUND_VACUUM_PAGES: u32 = 1024;
const CHECKPOINT_CACHE_BYTES: usize = 64 * 1024 * 1024;

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
    storage_probe: Option<Arc<dyn Fn(bool) -> bool + Send + Sync>>,
    clock: Arc<dyn LeaseClock>,
    domain: LeaseDomain,
    sender: ActorSender,
    usage_snapshot: Arc<Mutex<Result<StoreUsage, SessionStoreError>>>,
    worker: Arc<Worker>,
    #[cfg(feature = "perf-instrumentation")]
    trace: Option<SqliteTraceCollector>,
}

struct Backend {
    connection: Connection,
    clock: Arc<dyn LeaseClock>,
    domain: LeaseDomain,
    limits: MemoryStoreLimits,
    state: BackendState,
    /// Host-local proof of who is holding which branch right now.
    holders: crate::holder::HolderClaims,
    gc_pending: bool,
    usage_snapshot: Option<Arc<Mutex<Result<StoreUsage, SessionStoreError>>>>,
    vacuum_pending: bool,
    wal_pending: bool,
    checkpoints: CheckpointCache,
    #[cfg(test)]
    job_hook: Option<Box<dyn FnMut(ExecutedJob) + Send>>,
    #[cfg(test)]
    capacity_fault: Option<(&'static str, std::io::Error)>,
    /// Receives `PRAGMA synchronous` inside each unsynced job and after every
    /// job and background step, with the job's label.
    #[cfg(test)]
    synchronous_probe: Option<Box<dyn FnMut(&'static str, i64) + Send>>,
    /// Fails the next restore of `synchronous=FULL` with this error.
    #[cfg(test)]
    full_restore_fault: Option<SessionStoreError>,
}

/// Whether the actor's connection still takes work.
#[derive(Clone, Copy)]
enum BackendState {
    // Defensive: SQLite currently restores this connection-local pragma
    // without allocation; fault injection exercises capacity refusal here.
    AwaitingFull,
    Open,
    /// `synchronous=FULL` could not be restored after an unsynced job, so no
    /// later job may run on this connection; only a reopen clears it.
    Poisoned,
    Shutdown(DurabilityReport),
}

impl SqliteSessionStore {
    /// Also pauses new starts while a colocated authoritative store retains
    /// a capacity-refused publication. The probe never authorizes a write.
    /// Storage waits are interrupted per handle above this store, never here.
    #[must_use]
    pub fn with_storage_probe(mut self, probe: Arc<dyn Fn(bool) -> bool + Send + Sync>) -> Self {
        self.storage_probe = Some(probe);
        self
    }

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

    /// Read current branch revisions through one read-only snapshot.
    /// Missing or unreadable individual branches have no revision.
    ///
    /// # Errors
    /// The database or snapshot could not be admitted for inspection.
    pub fn inspect_branch_revisions(
        path: impl AsRef<Path>,
        selectors: &[SessionRef],
    ) -> Result<Vec<Option<u64>>, SessionStoreError> {
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        Ok(selectors
            .iter()
            .map(|selector| {
                branch_row(&snapshot, *selector)
                    .ok()
                    .flatten()
                    .map(|branch| branch.head.revision().get())
            })
            .collect())
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

    /// Opens with explicit actor and retained-state bounds on the system
    /// lease clock.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe I/O, schema, or corruption error.
    pub fn open_with_options(
        path: impl AsRef<Path>,
        options: SqliteStoreOptions,
    ) -> Result<Self, SessionStoreError> {
        Self::open_with_clock_and_options(path, Arc::new(SystemLeaseClock::new()?), options)
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
            storage_probe: None,
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
            let changes_before = backend.connection.total_changes();
            #[cfg(test)]
            let capacity_fault = if backend
                .capacity_fault
                .as_ref()
                .is_some_and(|(site, _)| *site == label)
            {
                backend
                    .capacity_fault
                    .take()
                    .map(|(_, error)| map_io(error))
            } else {
                None
            };
            #[cfg(not(test))]
            let capacity_fault = None;
            let restored = backend.resume_durability();
            let result = if let Err(error) = restored {
                Err(error)
            } else if let Some(error) = capacity_fault {
                Err(error)
            } else if matches!(backend.state, BackendState::Poisoned) {
                Err(SessionStoreError::BackendUnavailable)
            } else {
                operation(backend)
            };
            if matches!(lane, Lane::Critical) {
                if result.is_ok()
                    && (backend.connection.total_changes() > changes_before
                        || matches!(
                            label,
                            "recover_commit"
                                | "recover_create"
                                | "recover_fork"
                                | "recover_delete"
                                | "recover_prune"
                        ))
                {
                    activity.storage_full.store(false, Ordering::Release);
                } else if matches!(result, Err(SessionStoreError::StorageFull)) {
                    activity.storage_full.store(true, Ordering::Release);
                }
            }
            #[cfg(test)]
            backend.probe_synchronous(label);
            if matches!(lane, Lane::Critical) {
                *snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = backend
                    .require_open()
                    .and_then(|()| usage(&backend.connection, backend.limits));
            }
            // A traced gate run cuts at every commit and needs one commit
            // order per run: deferred reclamation this job left pending runs
            // right after it, before its reply, not whenever the actor idles.
            #[cfg(feature = "commit-trace")]
            {
                drop(settle);
                if crate::commit_trace::active() {
                    while backend.background_step() {}
                }
            }
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

#[cfg(feature = "perf-instrumentation")]
fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Clone for SqliteSessionStore {
    fn clone(&self) -> Self {
        Self {
            storage_probe: self.storage_probe.clone(),
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
        let (connection, tracing) = {
            let mut connection = connection;
            let tracing =
                crate::commit_trace::install(&mut connection, "runtime").map_err(map_sqlite)?;
            (connection, tracing)
        };
        #[cfg(all(test, feature = "commit-trace"))]
        let connection = {
            let mut connection = connection;
            if !tracing {
                install_test_commit_hook(&mut connection);
            }
            connection
        };
        #[cfg(all(test, not(feature = "commit-trace")))]
        let connection = {
            let mut connection = connection;
            install_test_commit_hook(&mut connection);
            connection
        };
        #[cfg(all(not(test), feature = "commit-trace"))]
        let _ = tracing;
        let gc_pending = gc::pending(&connection)?;
        Ok(Self {
            connection,
            clock,
            domain,
            limits,
            state: BackendState::Open,
            holders: crate::holder::HolderClaims::beside(path),
            gc_pending,
            usage_snapshot: None,
            vacuum_pending,
            wal_pending: vacuum_pending,
            checkpoints: CheckpointCache::new(CHECKPOINT_CACHE_BYTES),
            #[cfg(test)]
            job_hook: None,
            #[cfg(test)]
            capacity_fault: None,
            #[cfg(test)]
            synchronous_probe: None,
            #[cfg(test)]
            full_restore_fault: None,
        })
    }

    fn require_open(&self) -> Result<(), SessionStoreError> {
        match self.state {
            BackendState::Open => Ok(()),
            BackendState::AwaitingFull => Err(SessionStoreError::StorageFull),
            BackendState::Poisoned => Err(SessionStoreError::BackendUnavailable),
            BackendState::Shutdown(_) => Err(SessionStoreError::Shutdown),
        }
    }

    /// Runs one lease renewal or revival under `synchronous=NORMAL`.
    ///
    /// Its commit is visible to every connection at once and survives a
    /// process crash. Only power loss can lose it, and the next boot retires
    /// the grant it extended either way; WAL keeps commit order, so the next
    /// `FULL` commit syncs it. `FULL` is restored before the job returns. A
    /// connection that cannot restore it is poisoned, so no later commit can
    /// run weakened.
    fn unsynced<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, SessionStoreError>,
    ) -> Result<T, SessionStoreError> {
        self.require_open()?;
        let result = self
            .connection
            .execute_batch("PRAGMA synchronous=NORMAL;")
            .map_err(map_sqlite)
            .and_then(|()| {
                #[cfg(test)]
                self.probe_synchronous("unsynced");
                operation(self)
            });
        if let Err(error) = self.restore_full() {
            if error == SessionStoreError::StorageFull {
                // No later operation may run at NORMAL. The renewal itself
                // may have committed; exact renewal repetition reconciles it.
                self.state = BackendState::AwaitingFull;
                return Err(error);
            }
            self.state = BackendState::Poisoned;
            tracing::error!(
                target: "plakous_store::sqlite_actor",
                "SQLite store could not restore synchronous=FULL; refusing all further work until reopened"
            );
            return Err(SessionStoreError::BackendUnavailable);
        }
        result
    }

    fn resume_durability(&mut self) -> Result<(), SessionStoreError> {
        if !matches!(self.state, BackendState::AwaitingFull) {
            return Ok(());
        }
        match self.restore_full() {
            Ok(()) => {
                self.state = BackendState::Open;
                Ok(())
            }
            Err(SessionStoreError::StorageFull) => Err(SessionStoreError::StorageFull),
            Err(_) => {
                self.state = BackendState::Poisoned;
                Err(SessionStoreError::BackendUnavailable)
            }
        }
    }

    fn restore_full(&mut self) -> Result<(), SessionStoreError> {
        #[cfg(test)]
        if let Some(error) = self.full_restore_fault.take() {
            return Err(error);
        }
        self.connection
            .execute_batch("PRAGMA synchronous=FULL;")
            .map_err(map_sqlite)
    }

    #[cfg(test)]
    fn probe_synchronous(&mut self, label: &'static str) {
        let Some(probe) = self.synchronous_probe.as_mut() else {
            return;
        };
        let level = self
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap_or(-1);
        probe(label, level);
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
        self.checkpoints.load(&self.connection, id, row)
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
        if !matches!(self.state, BackendState::Open)
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
        #[cfg(test)]
        self.probe_synchronous("background");
        true
    }
}

impl SessionStore for SqliteSessionStore {
    fn recover_storage(&self) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            if let Some(probe) = self.storage_probe.clone()
                && tokio::task::spawn_blocking(move || probe(true))
                    .await
                    .map_err(|_| SessionStoreError::BackendUnavailable)?
            {
                return Err(SessionStoreError::StorageFull);
            }
            self.dispatch("recover_storage", |backend| {
                let tx = backend.connection.transaction().map_err(map_sqlite)?;
                tx.execute_batch(
                    "UPDATE store_meta SET sessions=sessions+1 WHERE id=1;
                    UPDATE store_meta SET sessions=sessions-1 WHERE id=1;",
                )
                .map_err(map_sqlite)?;
                tx.commit().map_err(map_sqlite)?;
                Ok(())
            })
            .await
        })
    }

    fn storage_unavailable(&self) -> bool {
        self.sender.activity.storage_full.load(Ordering::Acquire)
            || self
                .storage_probe
                .as_ref()
                .is_some_and(|probe| probe(false))
    }

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
            b.unsynced(|b| renew(b, &l))
        })
    }
    fn revive(&self, l: BranchLease) -> StoreFuture<'_, BranchLease> {
        self.dispatch("revive", move |b| {
            #[cfg(feature = "commit-trace")]
            let _label = crate::commit_trace::label("revive");
            b.unsynced(|b| revive(b, &l))
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
                let (_label, _classes) = (
                    crate::commit_trace::label("commit"),
                    crate::commit_trace::checkpoint_classes(r.checkpoint())?,
                );
                let queue_wait_ns = elapsed_ns(queued_at);
                let (receipt, execution) =
                    b.retry_after_reclaim(|b| ops::commit_observed(b, &r))?;
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
            let (_label, _classes) = (
                crate::commit_trace::label("commit"),
                crate::commit_trace::checkpoint_classes(r.checkpoint())?,
            );
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
    fn branch_line(&self, session: SessionRef) -> StoreFuture<'_, plakous_agent::internal::LineId> {
        self.dispatch_read("branch_line", move |b| {
            b.require_open()?;
            lines::branch_line(&b.connection, session)?
                .map(|(line, _)| line)
                .ok_or(SessionStoreError::NotFound)
        })
    }
    fn load_line(
        &self,
        line: plakous_agent::internal::LineId,
    ) -> StoreFuture<'_, plakous_agent::internal::LineRecord> {
        self.dispatch_read("load_line", move |b| {
            b.require_open()?;
            lines::load_line(&b.connection, line)
        })
    }
    fn line_job_terminals(
        &self,
        line: plakous_agent::internal::LineId,
    ) -> StoreFuture<'_, Vec<plakous_agent::internal::LineJobTerminal>> {
        self.dispatch_read("line_job_terminals", move |b| {
            b.require_open()?;
            let snapshot = b.connection.transaction().map_err(map_sqlite)?;
            lines::line_job_terminals(&snapshot, line)
        })
    }
    fn record_line_job_terminals(
        &self,
        r: plakous_agent::internal::RecordLineJobTerminals,
    ) -> StoreFuture<'_, plakous_agent::internal::LineTerminalsReceipt> {
        self.dispatch("record_line_job_terminals", move |b| {
            let receipt = lines::record_line_job_terminals(b, &r)?;
            feed::trim(b, r.lease().session().session_id());
            Ok(receipt)
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
            if let BackendState::Shutdown(r) = b.state {
                return Ok(r);
            }
            b.require_open()?;
            checkpoint_wal(&b.connection, true)?;
            let r = report();
            b.state = BackendState::Shutdown(r);
            Ok(r)
        })
    }
}

#[cfg(test)]
const CRASH_CUT_STAGE_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_CUT";
#[cfg(test)]
const CRASH_CUT_MARKER_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_MARKER";

#[cfg(test)]
fn install_test_commit_hook(connection: &mut Connection) {
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
