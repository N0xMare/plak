//! Bounded process-memory implementation of the semantic session store.

#![forbid(unsafe_code)]

/// Repository-only, process-wide durable SQLite commit tracing.
#[cfg(feature = "commit-trace")]
pub mod commit_trace;

#[cfg(feature = "sqlite")]
mod holder;
mod lease_clock;
use lease_clock::SystemLeaseClock;
mod memory_feed;
pub use memory_feed::MemoryFeedLimits;
use memory_feed::SessionFeed;

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::{
    BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS, BUNDLED_SQLITE_SOURCE_ID, BUNDLED_SQLITE_VERSION,
    SQLITE_SCHEMA_VERSION, SqliteFeedUsage, SqliteFileUsage, SqlitePersistenceShape,
    SqliteSessionStore, SqliteStoreOptions, SqliteStoreStats,
};
#[cfg(feature = "perf-instrumentation")]
#[doc(hidden)]
pub use sqlite::{SqliteCommitTrace, SqliteTraceCollector};

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{Arc, Mutex, MutexGuard};

use plakous_agent::internal::{
    AcquireBranch, ArchiveReceipt, ArchiveSession, BranchHead, BranchLease, BranchRevision,
    BranchTransitionMode, CheckpointId, CheckpointObjectBundle, CheckpointObjectRef,
    CommitCheckpoint, CommitOperation, CommitReceipt, CommittedCheckpoint, CreateOperation,
    CreateReceipt, CreateSession, DeleteReceipt, DeleteSession, DigestDomain, DurabilityReport,
    FeedBranchTransition, FeedChange, FeedHeadKind, FeedLocator, FeedPage, FeedSeq, FeedTail,
    ForkBranch, ForkLineage, ForkOperation, ForkReceipt, LeaseClock, LeaseDomain, LeaseInstant,
    LoadedBranch, Page, PruneBranch, PruneReceipt, RecoverFork, RuntimeCheckpoint,
    RuntimeCheckpointKind, RuntimeContextTransitionKind, RuntimeDurability, RuntimeLifecycle,
    RuntimeObject, RuntimeObjectAddress, SessionHead, SessionId, SessionQuery, SessionRef,
    SessionRevision, SessionStore, SessionStoreError, SessionSummary, SessionWorkflowId,
    StoreAdmission, StoreCapacityBound, StoreFuture, StoreInvalidReason, StoreReclaimMode,
    StoreReclaimReport, StoreUsage, StoredCheckpoint,
};

/// Default maximum retained sessions.
pub const DEFAULT_MAX_SESSIONS: usize = 1_024;
/// Default maximum branches in one session.
pub const DEFAULT_MAX_BRANCHES_PER_SESSION: usize = 1_024;
/// Default maximum retained immutable checkpoints.
pub const DEFAULT_MAX_CHECKPOINTS: usize = 131_072;
/// Default maximum unique content-addressed objects.
pub const DEFAULT_MAX_OBJECTS: usize = 1_048_576;
/// Default maximum aggregate unique object bytes.
pub const DEFAULT_MAX_OBJECT_BYTES: usize = 1024 * 1024 * 1024;
/// Default maximum listing page size.
pub const DEFAULT_MAX_PAGE_SIZE: usize = 256;

/// Hard process-memory bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryStoreLimits {
    sessions: NonZeroUsize,
    branches_per_session: NonZeroUsize,
    checkpoints: NonZeroUsize,
    objects: NonZeroUsize,
    object_bytes: NonZeroUsize,
    page_size: NonZeroUsize,
    feed: MemoryFeedLimits,
}

/// Exact bounded-retention counters for `perf-store` and conformance evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryStoreStats {
    sessions: usize,
    branches: usize,
    checkpoints: usize,
    unique_objects: usize,
    unique_object_bytes: usize,
    hot_bundles: usize,
    tombstones: usize,
    feed_trims: u64,
}

impl MemoryStoreStats {
    /// Returns retained sessions.
    #[must_use]
    pub const fn sessions(self) -> usize {
        self.sessions
    }

    /// Returns retained independent branch heads.
    #[must_use]
    pub const fn branches(self) -> usize {
        self.branches
    }

    /// Returns retained immutable checkpoint roots.
    #[must_use]
    pub const fn checkpoints(self) -> usize {
        self.checkpoints
    }

    /// Returns unique content-addressed objects.
    #[must_use]
    pub const fn unique_objects(self) -> usize {
        self.unique_objects
    }

    /// Returns conservative unique object and edge bytes.
    #[must_use]
    pub const fn unique_object_bytes(self) -> usize {
        self.unique_object_bytes
    }

    /// Returns strongly retained incremental bundles for current branch heads.
    #[must_use]
    pub const fn hot_bundles(self) -> usize {
        self.hot_bundles
    }

    /// Returns retained deletion tombstones.
    #[must_use]
    pub const fn tombstones(self) -> usize {
        self.tombstones
    }

    /// Returns the feed trim transactions run so far, each deleting at most
    /// one batch.
    #[must_use]
    pub const fn feed_trims(self) -> u64 {
        self.feed_trims
    }
}

impl Default for MemoryStoreLimits {
    fn default() -> Self {
        Self {
            sessions: nonzero(DEFAULT_MAX_SESSIONS),
            branches_per_session: nonzero(DEFAULT_MAX_BRANCHES_PER_SESSION),
            checkpoints: nonzero(DEFAULT_MAX_CHECKPOINTS),
            objects: nonzero(DEFAULT_MAX_OBJECTS),
            object_bytes: nonzero(DEFAULT_MAX_OBJECT_BYTES),
            page_size: nonzero(DEFAULT_MAX_PAGE_SIZE),
            feed: MemoryFeedLimits::default(),
        }
    }
}

impl MemoryStoreLimits {
    /// Creates explicit nonzero semantic/object bounds.
    #[must_use]
    pub const fn new(
        sessions: NonZeroUsize,
        branches_per_session: NonZeroUsize,
        checkpoints: NonZeroUsize,
        objects: NonZeroUsize,
        object_bytes: NonZeroUsize,
        page_size: NonZeroUsize,
    ) -> Self {
        Self {
            sessions,
            branches_per_session,
            checkpoints,
            objects,
            object_bytes,
            page_size,
            feed: MemoryFeedLimits::STANDARD,
        }
    }

    pub(crate) const fn feed_limits(self) -> MemoryFeedLimits {
        self.feed
    }

    /// Replaces the per-session feed retention bounds.
    #[must_use]
    pub const fn with_feed_limits(mut self, feed: MemoryFeedLimits) -> Self {
        self.feed = feed;
        self
    }

    /// Returns the retained-session bound.
    #[must_use]
    pub const fn max_sessions(self) -> NonZeroUsize {
        self.sessions
    }

    /// Returns the per-session branch bound.
    #[must_use]
    pub const fn max_branches_per_session(self) -> NonZeroUsize {
        self.branches_per_session
    }

    /// Returns the retained-checkpoint bound.
    #[must_use]
    pub const fn max_checkpoints(self) -> NonZeroUsize {
        self.checkpoints
    }

    /// Returns the unique-object bound.
    #[must_use]
    pub const fn max_objects(self) -> NonZeroUsize {
        self.objects
    }

    /// Returns the aggregate unique-object byte bound.
    #[must_use]
    pub const fn max_object_bytes(self) -> NonZeroUsize {
        self.object_bytes
    }

    /// Returns the listing page-size bound.
    #[must_use]
    pub const fn max_page_size(self) -> NonZeroUsize {
        self.page_size
    }
}

/// Bounded process-memory semantic store.
///
/// The authoritative retained representation is a deduplicated object graph,
/// bounded branch heads, and exact operation receipts. Materialized checkpoints
/// are retained only as validated memory caches and are never an alternate
/// persistence format.
#[derive(Clone)]
pub struct MemorySessionStore {
    state: Arc<Mutex<StoreState>>,
    clock: Arc<dyn LeaseClock>,
    domain: LeaseDomain,
    limits: MemoryStoreLimits,
}

impl MemorySessionStore {
    /// Creates an empty store using continuous system lease time.
    ///
    /// # Errors
    /// Returns a backend error when the host clock domain cannot be established.
    pub fn new(limits: MemoryStoreLimits) -> Result<Self, SessionStoreError> {
        Self::with_clock(limits, Arc::new(SystemLeaseClock::new()?))
    }

    /// Creates an empty store with an explicit, fixed lease clock domain.
    ///
    /// # Errors
    /// Returns the injected clock's initialization error.
    pub fn with_clock(
        limits: MemoryStoreLimits,
        clock: Arc<dyn LeaseClock>,
    ) -> Result<Self, SessionStoreError> {
        let domain = clock.now()?.domain();
        Ok(Self {
            state: Arc::new(Mutex::new(StoreState::default())),
            clock,
            domain,
            limits,
        })
    }

    /// Returns exact current retained-state counters without changing authority.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError::BackendUnavailable`] if authoritative
    /// in-process state is unavailable.
    pub fn stats(&self) -> Result<MemoryStoreStats, SessionStoreError> {
        let state = self.lock()?;
        Ok(MemoryStoreStats {
            sessions: state.sessions.len(),
            branches: state
                .sessions
                .values()
                .map(|session| session.branches.len())
                .sum(),
            checkpoints: state.checkpoints.len(),
            unique_objects: state.objects.len(),
            unique_object_bytes: state.object_bytes,
            hot_bundles: state.hot_bundles.len(),
            tombstones: state.deletes.len(),
            feed_trims: state.feed_trims,
        })
    }

    fn create_sync(&self, request: &CreateSession) -> Result<CreateReceipt, SessionStoreError> {
        let reference = bundle_reference(request.initial());
        let mut state = self.lock()?;
        require_open(&state)?;
        if state.deletes.contains_key(&request.session().session_id()) {
            return Err(SessionStoreError::IdentityConflict);
        }
        if let Some(receipt) = state.creates.get(&request.session().session_id()) {
            return if create_matches(receipt, request, reference) {
                Ok(receipt.clone())
            } else {
                Err(SessionStoreError::IdentityConflict)
            };
        }
        if state.sessions.contains_key(&request.session().session_id()) {
            return Err(SessionStoreError::IdentityConflict);
        }
        if state.sessions.len() >= self.limits.sessions.get() {
            return Err(capacity(
                StoreCapacityBound::Sessions,
                state.sessions.len(),
                self.limits.sessions.get(),
            ));
        }

        let candidate = publication_candidate(request.initial())?;
        validate_genesis(&candidate, request)?;
        let expires_at = expiry(self.lease_now()?, request.lease_seconds())?;
        let fence = NonZeroU64::MIN;
        let lease = BranchLease::from_store(
            request.session(),
            request.owner(),
            fence,
            expires_at,
            request.lease_seconds(),
        );
        let branch = BranchHead::from_store(
            request.session(),
            BranchRevision::INITIAL,
            candidate.reference,
        );
        let session = SessionHead::from_store(
            request.session().session_id(),
            request.session().branch_id(),
            SessionRevision::INITIAL,
            request.created_at(),
            RuntimeDurability::Disabled,
            false,
        );
        let operation = CreateOperation::from_store(
            request.session(),
            request.created_at(),
            request.owner(),
            request.lease_seconds(),
            candidate.reference,
            candidate.checkpoint.checkpoint_commit_identity(),
        );
        let record = memory_feed::prepare(
            &state.feeds,
            request.session().session_id(),
            request.session().branch_id(),
            FeedChange::Created {
                genesis: candidate.reference.checkpoint_id(),
                revision: BranchRevision::INITIAL,
            },
            request.feed_facts(),
        )?;
        let receipt =
            CreateReceipt::from_store(operation, session, branch, lease.clone(), record.seq());
        let hot_bundle =
            install_candidate(&mut state, candidate, self.limits, request.admission())?;
        memory_feed::append(&mut state.feeds, record);
        state.sessions.insert(
            request.session().session_id(),
            SessionRecord {
                head: session,
                branches: BTreeMap::from([(
                    request.session().branch_id(),
                    BranchRecord {
                        head: branch,
                        last_fence: fence.get(),
                        active_lease: Some(lease),
                        previous_renewal: None,
                        released_lease: None,
                    },
                )]),
                archive_receipt: None,
            },
        );
        state
            .creates
            .insert(request.session().session_id(), receipt.clone());
        retain_head_bundle(&mut state, hot_bundle);
        Ok(receipt)
    }

    fn recover_create_sync(
        &self,
        request: &CreateSession,
    ) -> Result<CreateReceipt, SessionStoreError> {
        let reference = bundle_reference(request.initial());
        let state = self.lock()?;
        match state.creates.get(&request.session().session_id()) {
            Some(receipt) if create_matches(receipt, request, reference) => Ok(receipt.clone()),
            Some(_) => Err(SessionStoreError::IdentityConflict),
            None => Err(SessionStoreError::NotFound),
        }
    }

    fn load_branch_sync(&self, selector: SessionRef) -> Result<LoadedBranch, SessionStoreError> {
        let state = self.lock()?;
        require_open(&state)?;
        let session = state
            .sessions
            .get(&selector.session_id())
            .ok_or(SessionStoreError::NotFound)?;
        if session.head.is_archived() {
            return Err(SessionStoreError::Archived);
        }
        let branch = session
            .branches
            .get(&selector.branch_id())
            .ok_or(SessionStoreError::NotFound)?;
        let checkpoint = stored_checkpoint(&state, branch.head.checkpoint())?;
        Ok(LoadedBranch::from_store(
            session.head,
            branch.head,
            checkpoint,
        ))
    }

    fn load_checkpoint_sync(
        &self,
        checkpoint_id: CheckpointId,
    ) -> Result<StoredCheckpoint, SessionStoreError> {
        let state = self.lock()?;
        let Some(record) = state.checkpoints.get(&checkpoint_id) else {
            return Err(if state.pruned.contains_key(&checkpoint_id) {
                SessionStoreError::Pruned
            } else {
                SessionStoreError::NotFound
            });
        };
        stored_checkpoint(&state, record.reference)
    }

    fn acquire_sync(&self, request: AcquireBranch) -> Result<BranchLease, SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        let now = self.lease_now()?;
        let branch = mutable_branch(&mut state, request.session())?;
        if let Some(current) = &branch.active_lease
            && !expired(current, now)?
        {
            return if current.owner() == request.owner()
                && current.lease_seconds() == request.lease_seconds()
            {
                Ok(current.clone())
            } else {
                Err(SessionStoreError::LeaseHeld)
            };
        }
        let fence = branch
            .last_fence
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                SessionStoreError::capacity(StoreCapacityBound::LeaseFence, branch.last_fence, 0)
            })?;
        let lease = BranchLease::from_store(
            request.session(),
            request.owner(),
            fence,
            expiry(now, request.lease_seconds())?,
            request.lease_seconds(),
        );
        branch.last_fence = fence.get();
        branch.active_lease = Some(lease.clone());
        branch.previous_renewal = None;
        branch.released_lease = None;
        Ok(lease)
    }

    fn renew_sync(&self, lease: BranchLease) -> Result<BranchLease, SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        let now = self.lease_now()?;
        require_lease_domain(&lease, now)?;
        let branch = mutable_branch(&mut state, lease.session())?;
        if let Some((prior, renewed)) = &branch.previous_renewal
            && *prior == lease
            && branch.active_lease.as_ref() == Some(renewed)
        {
            return if expired(renewed, now)? {
                Err(SessionStoreError::LeaseExpired)
            } else {
                Ok(renewed.clone())
            };
        }
        let current = branch
            .active_lease
            .as_ref()
            .ok_or(SessionStoreError::FenceSuperseded)?;
        if current != &lease {
            return Err(SessionStoreError::FenceSuperseded);
        }
        if expired(current, now)? {
            return Err(SessionStoreError::LeaseExpired);
        }
        let expires_at = expiry(now, lease.lease_seconds())?;
        if expires_at.same_domain_cmp(current.expires_at())? != std::cmp::Ordering::Greater {
            return Ok(current.clone());
        }
        let renewed = BranchLease::from_store(
            lease.session(),
            lease.owner(),
            lease.fence(),
            expires_at,
            lease.lease_seconds(),
        );
        branch.previous_renewal = Some((lease, renewed.clone()));
        branch.active_lease = Some(renewed.clone());
        Ok(renewed)
    }

    fn revive_sync(&self, lease: BranchLease) -> Result<BranchLease, SessionStoreError> {
        let now = self.lease_now()?;
        require_lease_domain(&lease, now)?;
        if !expired(&lease, now)? {
            return self.renew_sync(lease);
        }
        let mut state = self.lock()?;
        require_open(&state)?;
        let branch = mutable_branch(&mut state, lease.session())?;
        if branch.active_lease.as_ref() != Some(&lease) || branch.last_fence != lease.fence().get()
        {
            return Err(SessionStoreError::FenceSuperseded);
        }
        let revived = BranchLease::from_store(
            lease.session(),
            lease.owner(),
            lease.fence(),
            expiry(now, lease.lease_seconds())?,
            lease.lease_seconds(),
        );
        branch.previous_renewal = Some((lease, revived.clone()));
        branch.active_lease = Some(revived.clone());
        Ok(revived)
    }

    fn release_sync(&self, lease: BranchLease) -> Result<(), SessionStoreError> {
        let mut guard = self.lock()?;
        let state = &mut *guard;
        require_open(state)?;
        let now = self.lease_now()?;
        require_lease_domain(&lease, now)?;
        let branch = mutable_branch(state, lease.session())?;
        if branch.released_lease.as_ref() == Some(&lease) && branch.active_lease.is_none() {
            return Ok(());
        }
        let current = branch
            .active_lease
            .as_ref()
            .ok_or(SessionStoreError::FenceSuperseded)?;
        if current != &lease {
            return Err(SessionStoreError::FenceSuperseded);
        }
        let head = branch.head.checkpoint().checkpoint_id();
        let change = FeedChange::LeaseReleased {
            head,
            fence: lease.fence().get(),
        };
        let record = memory_feed::prepare_change(&state.feeds, lease.session(), change)?;
        let branch = mutable_branch(state, lease.session())?;
        branch.active_lease = None;
        branch.previous_renewal = None;
        branch.released_lease = Some(lease);
        memory_feed::append(&mut state.feeds, record);
        Ok(())
    }

    fn commit_sync(&self, request: &CommitCheckpoint) -> Result<CommitReceipt, SessionStoreError> {
        let reference = bundle_reference(request.checkpoint());
        let checkpoint_id = reference.checkpoint_id();
        let mut state = self.lock()?;
        require_open(&state)?;
        if let Some(committed) = state.commits.get(&checkpoint_id) {
            return if commit_matches(committed, request, reference) {
                Ok(committed.receipt())
            } else {
                Err(SessionStoreError::IdentityConflict)
            };
        }
        if commit_history_pruned(&state, checkpoint_id) {
            return Err(SessionStoreError::Pruned);
        }
        let candidate = publication_candidate(request.checkpoint())?;
        let expected_checkpoint = Arc::clone(
            &state
                .checkpoints
                .get(&request.expected().checkpoint().checkpoint_id())
                .ok_or(SessionStoreError::corrupt())?
                .checkpoint,
        );
        validate_commit(&candidate, request, &expected_checkpoint)?;
        let now = self.lease_now()?;
        {
            let branch = branch(&state, request.expected().session())?;
            require_lease(branch, request.lease(), now)?;
            if branch.head != request.expected() {
                return Err(SessionStoreError::Conflict);
            }
        }
        let next_revision = request
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
        let head = BranchHead::from_store(
            request.expected().session(),
            next_revision,
            candidate.reference,
        );
        let operation = CommitOperation::from_store(
            request.lease().clone(),
            request.expected(),
            candidate.reference,
            candidate.checkpoint.checkpoint_commit_identity(),
        );
        let record = memory_feed::prepare(
            &state.feeds,
            request.expected().session().session_id(),
            request.expected().session().branch_id(),
            FeedChange::Advanced {
                parent: request.expected().checkpoint().checkpoint_id(),
                checkpoint: candidate.reference.checkpoint_id(),
                revision: next_revision,
                head: match candidate.checkpoint.kind() {
                    RuntimeCheckpointKind::Quiescent => FeedHeadKind::Quiescent,
                    RuntimeCheckpointKind::Working => FeedHeadKind::Working,
                },
            },
            request.feed_facts(),
        )?;
        let receipt =
            CommitReceipt::from_store(candidate.reference, request.expected(), head, record.seq());
        let committed = CommittedCheckpoint::from_store(operation, receipt);
        let hot_bundle =
            install_candidate(&mut state, candidate, self.limits, request.admission())?;
        mutable_branch(&mut state, request.expected().session())?.head = head;
        memory_feed::append(&mut state.feeds, record);
        state.commits.insert(checkpoint_id, committed);
        retain_head_bundle(&mut state, hot_bundle);
        Ok(receipt)
    }

    fn recover_commit_sync(
        &self,
        checkpoint: CheckpointId,
    ) -> Result<CommittedCheckpoint, SessionStoreError> {
        let state = self.lock()?;
        match state.commits.get(&checkpoint) {
            Some(committed) => Ok(committed.clone()),
            None if commit_history_pruned(&state, checkpoint) => Err(SessionStoreError::Pruned),
            None => Err(SessionStoreError::NotFound),
        }
    }

    fn prune_sync(&self, request: &PruneBranch) -> Result<PruneReceipt, SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        let now = self.lease_now()?;
        let selector = request.expected().session();
        if state
            .sessions
            .get(&selector.session_id())
            .ok_or(SessionStoreError::NotFound)?
            .head
            .is_archived()
        {
            return Err(SessionStoreError::Archived);
        }
        let head = {
            let branch = branch(&state, selector)?;
            require_lease(branch, request.lease(), now)?;
            if branch.head != request.expected() {
                return Err(SessionStoreError::Conflict);
            }
            branch.head
        };
        let checkpoint = Arc::clone(
            &state
                .checkpoints
                .get(&head.checkpoint().checkpoint_id())
                .ok_or(SessionStoreError::corrupt())?
                .checkpoint,
        );
        if checkpoint.kind() != RuntimeCheckpointKind::Quiescent {
            return Err(SessionStoreError::Invalid(
                StoreInvalidReason::PruneHeadNotQuiescent,
            ));
        }
        let unchanged = PruneReceipt::from_store(
            head,
            state
                .horizons
                .get(&selector)
                .copied()
                .unwrap_or(BranchRevision::INITIAL),
            0,
        );
        let Some(horizon) = head.revision().get().checked_sub(1) else {
            return Ok(unchanged);
        };
        let pinned = pinned_checkpoints(&state, selector);
        let superseded = state
            .checkpoints
            .iter()
            .filter_map(|(id, record)| {
                let topology = record.checkpoint.manifest().topology();
                (topology.session_id == selector.session_id()
                    && topology.branch_id == selector.branch_id()
                    && topology.branch_revision < horizon
                    && !pinned.contains(id)
                    && !request.retain().contains(id))
                .then_some(*id)
            })
            .collect::<BTreeSet<_>>();
        let pruned = count(superseded.len());
        if pruned == 0 || pruned < request.minimum() {
            return Ok(unchanged);
        }
        // Plan the sweep against surviving roots before changing any authority.
        // Publication below is infallible and stays under the commit lock.
        let sweep = plan_object_sweep(&state, &superseded)?;
        let horizon_revision = BranchRevision::from_store(horizon);
        let change = FeedChange::Pruned {
            head: head.checkpoint().checkpoint_id(),
            horizon: horizon_revision,
        };
        let record = memory_feed::prepare_change(&state.feeds, selector, change)?;
        memory_feed::append(&mut state.feeds, record);
        state.commits.retain(|_, committed| {
            let receipt = committed.receipt();
            receipt.head().session() != selector || receipt.head().revision().get() > horizon
        });
        for id in &superseded {
            state.checkpoints.remove(id);
            state.pruned.insert(*id, selector.session_id());
        }
        state.horizons.insert(selector, horizon_revision);
        // Surviving warm bundles may still remember addresses this sweep
        // reclaims. A later load must rebuild from the retained graph instead.
        state.hot_bundles.clear();
        for address in sweep.objects {
            state.objects.remove(&address);
        }
        state.object_bytes = sweep.remaining_bytes;
        Ok(PruneReceipt::from_store(head, horizon_revision, pruned))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one atomic fork validates and publishes the complete old/new authority tuple"
    )]
    fn fork_sync(&self, request: &ForkBranch) -> Result<ForkReceipt, SessionStoreError> {
        let target = SessionRef::new(
            request.source().session().session_id(),
            request.target_branch(),
        );
        let mut state = self.lock()?;
        require_open(&state)?;
        validate_fork_semantic_request(request)?;
        let candidate = publication_candidate(request.target())?;
        if let Some(receipt) = state.forks.get(&request.workflow_id()) {
            return if fork_matches(receipt, request, &candidate) {
                Ok(receipt.clone())
            } else {
                Err(SessionStoreError::IdentityConflict)
            };
        }
        let source_checkpoint = {
            let session = state
                .sessions
                .get(&target.session_id())
                .ok_or(SessionStoreError::NotFound)?;
            if session.head.is_archived() {
                return Err(SessionStoreError::Archived);
            }
            if session.head.revision() != request.expected_session_revision() {
                return Err(SessionStoreError::Conflict);
            }
            if session.branches.contains_key(&target.branch_id()) {
                return Err(SessionStoreError::IdentityConflict);
            }
            if session.branches.len() >= self.limits.branches_per_session.get() {
                return Err(capacity(
                    StoreCapacityBound::BranchesPerSession,
                    session.branches.len(),
                    self.limits.branches_per_session.get(),
                ));
            }
            let source = session
                .branches
                .get(&request.source().session().branch_id())
                .ok_or(SessionStoreError::NotFound)?;
            require_lease(source, request.source_lease(), self.lease_now()?)?;
            if source.head != request.source() {
                return Err(SessionStoreError::Conflict);
            }
            Arc::clone(
                &state
                    .checkpoints
                    .get(&source.head.checkpoint().checkpoint_id())
                    .ok_or(SessionStoreError::corrupt())?
                    .checkpoint,
            )
        };
        if source_checkpoint.kind() != RuntimeCheckpointKind::Quiescent {
            return Err(SessionStoreError::Invalid(
                StoreInvalidReason::ForkSourceNotQuiescent,
            ));
        }
        validate_fork(&candidate, &source_checkpoint, request, target)?;
        let next_revision = request
            .expected_session_revision()
            .checked_next()
            .ok_or_else(|| {
                SessionStoreError::capacity(
                    StoreCapacityBound::SessionRevision,
                    request.expected_session_revision().get(),
                    0,
                )
            })?;
        let fence = NonZeroU64::MIN;
        let lease = BranchLease::from_store(
            target,
            request.owner(),
            fence,
            expiry(self.lease_now()?, request.lease_seconds())?,
            request.lease_seconds(),
        );
        let target_head =
            BranchHead::from_store(target, BranchRevision::INITIAL, candidate.reference);
        let previous_session = state
            .sessions
            .get(&target.session_id())
            .ok_or(SessionStoreError::NotFound)?
            .head;
        let session_head = SessionHead::from_store(
            previous_session.session_id(),
            previous_session.initial_branch(),
            next_revision,
            previous_session.created_at(),
            previous_session.durability(),
            false,
        );
        let operation = ForkOperation::from_store(
            request.workflow_id(),
            request.semantic_request(),
            request.expected_session_revision(),
            request.source(),
            target,
            request.transition(),
            candidate.reference,
            candidate.checkpoint.checkpoint_commit_identity(),
        );
        let source = request.source();
        let record = memory_feed::prepare(
            &state.feeds,
            target.session_id(),
            target.branch_id(),
            FeedChange::Branched {
                source: FeedLocator {
                    session_id: source.session().session_id(),
                    branch_id: source.session().branch_id(),
                    checkpoint_id: source.checkpoint().checkpoint_id(),
                },
                target: candidate.reference.checkpoint_id(),
                revision: next_revision,
                transition: match request.transition() {
                    BranchTransitionMode::OrdinaryFork => FeedBranchTransition::Fork,
                    BranchTransitionMode::Rebind => FeedBranchTransition::Rebind,
                },
            },
            request.feed_facts(),
        )?;
        let receipt = ForkReceipt::from_store(
            operation,
            previous_session,
            session_head,
            request.source(),
            target_head,
            lease.clone(),
            record.seq(),
        );
        let hot_bundle =
            install_candidate(&mut state, candidate, self.limits, request.admission())?;
        memory_feed::append(&mut state.feeds, record);
        let session = state
            .sessions
            .get_mut(&target.session_id())
            .ok_or(SessionStoreError::NotFound)?;
        session.head = session_head;
        session.branches.insert(
            target.branch_id(),
            BranchRecord {
                head: target_head,
                last_fence: fence.get(),
                active_lease: Some(lease),
                previous_renewal: None,
                released_lease: None,
            },
        );
        state.forks.insert(request.workflow_id(), receipt.clone());
        retain_head_bundle(&mut state, hot_bundle);
        Ok(receipt)
    }

    fn recover_fork_sync(&self, request: &RecoverFork) -> Result<ForkReceipt, SessionStoreError> {
        let state = self.lock()?;
        let receipt = state
            .forks
            .get(&request.workflow_id())
            .ok_or(SessionStoreError::NotFound)?;
        if !recover_fork_matches(receipt, request) {
            return Err(SessionStoreError::IdentityConflict);
        }
        let session = state
            .sessions
            .get(&request.source().session().session_id())
            .ok_or(SessionStoreError::NotFound)?;
        let branch = session
            .branches
            .get(&request.source().session().branch_id())
            .ok_or(SessionStoreError::NotFound)?;
        require_lease(branch, request.source_lease(), self.lease_now()?)?;
        let source = stored_checkpoint(&state, receipt.source().checkpoint())?;
        let target = stored_checkpoint(&state, receipt.target().checkpoint())?;
        validate_materialized_fork_receipt(receipt, &source, &target)?;
        Ok(receipt.clone())
    }

    fn list_sync(&self, query: SessionQuery) -> Result<Page<SessionSummary>, SessionStoreError> {
        let state = self.lock()?;
        require_open(&state)?;
        if query.limit().get() > self.limits.page_size.get() {
            return Err(capacity(
                StoreCapacityBound::PageSize,
                query.limit().get(),
                self.limits.page_size.get(),
            ));
        }
        let mut matched = state
            .sessions
            .iter()
            .filter(|(id, session)| {
                query.after_session().is_none_or(|after| **id > after)
                    && (query.includes_archived() || !session.head.is_archived())
            })
            .map(|(_, session)| {
                SessionSummary::from_store(session.head, count(session.branches.len()))
            })
            .take(query.limit().get() + 1)
            .collect::<Vec<_>>();
        let has_more = matched.len() > query.limit().get();
        if has_more {
            matched.pop();
        }
        let next_after = has_more
            .then(|| matched.last().map(|summary| summary.head().session_id()))
            .flatten();
        Ok(Page::from_store(matched, next_after))
    }

    fn archive_sync(&self, request: ArchiveSession) -> Result<ArchiveReceipt, SessionStoreError> {
        let mut guard = self.lock()?;
        let state = &mut *guard;
        require_open(state)?;
        let now = self.lease_now()?;
        let session = state
            .sessions
            .get_mut(&request.session_id())
            .ok_or(SessionStoreError::NotFound)?;
        if let Some(receipt) = session.archive_receipt {
            return if receipt.request() == request {
                Ok(receipt)
            } else {
                Err(SessionStoreError::Archived)
            };
        }
        if session.head.revision() != request.expected_revision() {
            return Err(SessionStoreError::Conflict);
        }
        for lease in session
            .branches
            .values()
            .filter_map(|branch| branch.active_lease.as_ref())
        {
            if !expired(lease, now)? {
                return Err(SessionStoreError::LeaseHeld);
            }
        }
        let revision = request.expected_revision().checked_next().ok_or_else(|| {
            SessionStoreError::capacity(
                StoreCapacityBound::SessionRevision,
                request.expected_revision().get(),
                0,
            )
        })?;
        let previous = session.head;
        let archived = SessionHead::from_store(
            previous.session_id(),
            previous.initial_branch(),
            revision,
            previous.created_at(),
            previous.durability(),
            true,
        );
        let receipt = ArchiveReceipt::from_store(request, previous, archived);
        let record = memory_feed::prepare_change(
            &state.feeds,
            SessionRef::new(previous.session_id(), previous.initial_branch()),
            FeedChange::Archived { revision },
        )?;
        memory_feed::append(&mut state.feeds, record);
        session.head = archived;
        session.archive_receipt = Some(receipt);
        for branch in session.branches.values_mut() {
            branch.active_lease = None;
            branch.previous_renewal = None;
        }
        prune_hot_bundles(state);
        Ok(receipt)
    }

    fn delete_sync(&self, request: DeleteSession) -> Result<DeleteReceipt, SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        if let Some(receipt) = state.deletes.get(&request.session_id()) {
            return if receipt.request() == request {
                Ok(*receipt)
            } else {
                Err(SessionStoreError::Conflict)
            };
        }
        let archived = state
            .sessions
            .get(&request.session_id())
            .ok_or(SessionStoreError::NotFound)?
            .head;
        if !archived.is_archived() {
            return Err(SessionStoreError::Invalid(
                StoreInvalidReason::SessionNotArchived,
            ));
        }
        if archived.revision() != request.expected_revision() {
            return Err(SessionStoreError::Conflict);
        }

        let removed_checkpoints = state
            .checkpoints
            .iter()
            .filter_map(|(id, record)| {
                (record.checkpoint.manifest().topology().session_id == request.session_id())
                    .then_some(*id)
            })
            .collect::<BTreeSet<_>>();
        let checkpoints_removed = count(removed_checkpoints.len());
        // Deletion has the same all-or-nothing contract as SQLite: stage the
        // graph sweep and publish it only after every fallible check succeeds.
        let mut staged = state.clone();
        staged.sessions.remove(&request.session_id());
        staged.feeds.remove(&request.session_id());
        staged.creates.remove(&request.session_id());
        staged.forks.retain(|_, receipt| {
            receipt.source().session().session_id() != request.session_id()
                && receipt.target().session().session_id() != request.session_id()
        });
        staged.commits.retain(|_, committed| {
            committed.operation().expected().session().session_id() != request.session_id()
        });
        staged
            .pruned
            .retain(|_, session| *session != request.session_id());
        staged
            .horizons
            .retain(|selector, _| selector.session_id() != request.session_id());
        for id in &removed_checkpoints {
            staged.checkpoints.remove(id);
            staged.hot_bundles.remove(id);
        }
        sweep_unreachable(&mut staged)?;
        let receipt = DeleteReceipt::from_store(request, archived, checkpoints_removed);
        staged.deletes.insert(request.session_id(), receipt);
        *state = staged;
        Ok(receipt)
    }

    fn recover_delete_sync(
        &self,
        request: DeleteSession,
    ) -> Result<DeleteReceipt, SessionStoreError> {
        let state = self.lock()?;
        match state.deletes.get(&request.session_id()) {
            Some(receipt) if receipt.request() == request => Ok(*receipt),
            Some(_) => Err(SessionStoreError::Conflict),
            None => Err(SessionStoreError::NotFound),
        }
    }

    fn flush_sync(&self) -> Result<DurabilityReport, SessionStoreError> {
        let state = self.lock()?;
        require_open(&state)?;
        Ok(memory_report())
    }

    fn shutdown_sync(&self) -> Result<DurabilityReport, SessionStoreError> {
        let mut state = self.lock()?;
        if let Some(report) = state.shutdown_report {
            return Ok(report);
        }
        let report = memory_report();
        state.shutdown_report = Some(report);
        Ok(report)
    }

    fn read_feed_sync(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> Result<FeedPage, SessionStoreError> {
        let state = self.lock()?;
        let session = state
            .sessions
            .get(&session_id)
            .ok_or(SessionStoreError::NotFound)?;
        let feed = state
            .feeds
            .get(&session_id)
            .ok_or(SessionStoreError::corrupt())?;
        Ok(memory_feed::read(
            feed,
            || {
                session
                    .branches
                    .values()
                    .map(|branch| branch.head)
                    .collect()
            },
            after,
            max_records,
        ))
    }

    fn feed_tail_sync(&self, session_id: SessionId) -> Result<FeedTail, SessionStoreError> {
        let state = self.lock()?;
        if !state.sessions.contains_key(&session_id) {
            return Err(SessionStoreError::NotFound);
        }
        state
            .feeds
            .get(&session_id)
            .map(memory_feed::tail)
            .ok_or(SessionStoreError::corrupt())
    }

    /// Trims one session's feed after a publishing mutation, one bounded
    /// batch per lock acquisition, so no trim ever runs inside a mutation.
    fn trim_feed(&self, session_id: SessionId) {
        let target = {
            let Ok(state) = self.lock() else {
                return;
            };
            state
                .feeds
                .get(&session_id)
                .and_then(|feed| memory_feed::trim_target(feed, self.limits.feed))
        };
        let Some(target) = target else {
            return;
        };
        loop {
            let Ok(mut guard) = self.lock() else {
                return;
            };
            let state = &mut *guard;
            let Some(feed) = state.feeds.get_mut(&session_id) else {
                return;
            };
            let more = memory_feed::trim_batch(feed, target, self.limits.feed);
            state.feed_trims = state.feed_trims.saturating_add(1);
            if !more {
                return;
            }
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, StoreState>, SessionStoreError> {
        self.state
            .lock()
            .map_err(|_| SessionStoreError::BackendUnavailable)
    }
}

impl SessionStore for MemorySessionStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        sample_clock(self.clock.as_ref(), self.domain)
    }

    fn create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        Box::pin(async move {
            let receipt = self.create_sync(&request)?;
            self.trim_feed(request.session().session_id());
            Ok(receipt)
        })
    }

    fn recover_create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        Box::pin(async move { self.recover_create_sync(&request) })
    }

    fn load_branch(&self, session: SessionRef) -> StoreFuture<'_, LoadedBranch> {
        Box::pin(async move { self.load_branch_sync(session) })
    }

    fn load_checkpoint(&self, checkpoint: CheckpointId) -> StoreFuture<'_, StoredCheckpoint> {
        Box::pin(async move { self.load_checkpoint_sync(checkpoint) })
    }

    fn acquire(&self, request: AcquireBranch) -> StoreFuture<'_, BranchLease> {
        Box::pin(async move { self.acquire_sync(request) })
    }

    fn renew(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        Box::pin(async move { self.renew_sync(lease) })
    }

    fn revive(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        Box::pin(async move { self.revive_sync(lease) })
    }

    fn release(&self, lease: BranchLease) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let session_id = lease.session().session_id();
            self.release_sync(lease)?;
            self.trim_feed(session_id);
            Ok(())
        })
    }

    fn commit(&self, request: CommitCheckpoint) -> StoreFuture<'_, CommitReceipt> {
        Box::pin(async move {
            let receipt = self.commit_sync(&request)?;
            self.trim_feed(request.expected().session().session_id());
            Ok(receipt)
        })
    }

    fn recover_commit(&self, checkpoint: CheckpointId) -> StoreFuture<'_, CommittedCheckpoint> {
        Box::pin(async move { self.recover_commit_sync(checkpoint) })
    }

    fn fork(&self, request: ForkBranch) -> StoreFuture<'_, ForkReceipt> {
        Box::pin(async move {
            let receipt = self.fork_sync(&request)?;
            self.trim_feed(request.source().session().session_id());
            Ok(receipt)
        })
    }

    fn recover_fork(&self, request: RecoverFork) -> StoreFuture<'_, ForkReceipt> {
        Box::pin(async move { self.recover_fork_sync(&request) })
    }

    fn list(&self, query: SessionQuery) -> StoreFuture<'_, Page<SessionSummary>> {
        Box::pin(async move { self.list_sync(query) })
    }

    fn reclaim(&self, _mode: StoreReclaimMode) -> StoreFuture<'_, StoreReclaimReport> {
        Box::pin(async move {
            let state = self.lock()?;
            require_open(&state)?;
            Ok(StoreReclaimReport::default())
        })
    }

    fn usage(&self) -> StoreFuture<'_, StoreUsage> {
        Box::pin(async move {
            let state = self.lock()?;
            Ok(StoreUsage::new(
                (
                    count(state.checkpoints.len()),
                    count(self.limits.checkpoints.get()),
                ),
                (count(state.objects.len()), count(self.limits.objects.get())),
                (
                    count(state.object_bytes),
                    count(self.limits.object_bytes.get()),
                ),
            ))
        })
    }

    fn archive(&self, request: ArchiveSession) -> StoreFuture<'_, ArchiveReceipt> {
        Box::pin(async move {
            let receipt = self.archive_sync(request)?;
            self.trim_feed(request.session_id());
            Ok(receipt)
        })
    }

    fn delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        Box::pin(async move { self.delete_sync(request) })
    }

    fn prune(&self, request: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
        Box::pin(async move {
            let receipt = self.prune_sync(&request)?;
            self.trim_feed(request.expected().session().session_id());
            Ok(receipt)
        })
    }

    fn read_feed(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> StoreFuture<'_, FeedPage> {
        Box::pin(async move { self.read_feed_sync(session_id, after, max_records) })
    }

    fn feed_tail(&self, session_id: SessionId) -> StoreFuture<'_, FeedTail> {
        Box::pin(async move { self.feed_tail_sync(session_id) })
    }

    fn recover_delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        Box::pin(async move { self.recover_delete_sync(request) })
    }

    fn flush(&self) -> StoreFuture<'_, DurabilityReport> {
        Box::pin(async move { self.flush_sync() })
    }

    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
        Box::pin(async move { self.shutdown_sync() })
    }
}

#[derive(Clone, Default)]
struct StoreState {
    sessions: BTreeMap<SessionId, SessionRecord>,
    checkpoints: BTreeMap<CheckpointId, CheckpointRecord>,
    objects: BTreeMap<RuntimeObjectAddress, RuntimeObject>,
    object_bytes: usize,
    hot_bundles: BTreeMap<CheckpointId, CheckpointObjectBundle>,
    creates: BTreeMap<SessionId, CreateReceipt>,
    forks: BTreeMap<SessionWorkflowId, ForkReceipt>,
    commits: BTreeMap<CheckpointId, CommittedCheckpoint>,
    deletes: BTreeMap<SessionId, DeleteReceipt>,
    pruned: BTreeMap<CheckpointId, SessionId>,
    horizons: BTreeMap<SessionRef, BranchRevision>,
    feeds: BTreeMap<SessionId, SessionFeed>,
    feed_trims: u64,
    shutdown_report: Option<DurabilityReport>,
}

#[derive(Clone)]
struct SessionRecord {
    head: SessionHead,
    branches: BTreeMap<plakous_agent::internal::BranchId, BranchRecord>,
    archive_receipt: Option<ArchiveReceipt>,
}

#[derive(Clone)]
struct BranchRecord {
    head: BranchHead,
    last_fence: u64,
    active_lease: Option<BranchLease>,
    previous_renewal: Option<(BranchLease, BranchLease)>,
    released_lease: Option<BranchLease>,
}

#[derive(Clone)]
struct CheckpointRecord {
    reference: CheckpointObjectRef,
    checkpoint: Arc<RuntimeCheckpoint>,
}

struct Candidate {
    reference: CheckpointObjectRef,
    base_root: Option<RuntimeObjectAddress>,
    checkpoint: Arc<RuntimeCheckpoint>,
    bundle: CheckpointObjectBundle,
    objects: Vec<RuntimeObject>,
}

fn publication_candidate(bundle: &CheckpointObjectBundle) -> Result<Candidate, SessionStoreError> {
    let checkpoint = bundle
        .validated_checkpoint()
        .map_err(|_| SessionStoreError::Invalid(StoreInvalidReason::CheckpointObjectBundle))?;
    let reference = bundle_reference(bundle);
    if checkpoint.manifest().topology().checkpoint_id != reference.checkpoint_id()
        || checkpoint.integrity() != reference.integrity()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointObjectBinding,
        ));
    }
    Ok(Candidate {
        reference,
        base_root: bundle.base_root(),
        checkpoint,
        bundle: bundle.clone(),
        objects: bundle.supplied_objects().to_vec(),
    })
}

fn bundle_reference(bundle: &CheckpointObjectBundle) -> CheckpointObjectRef {
    CheckpointObjectRef::from_store(
        bundle.checkpoint_id(),
        bundle.root(),
        bundle.checkpoint_integrity(),
    )
}

fn install_candidate(
    state: &mut StoreState,
    candidate: Candidate,
    limits: MemoryStoreLimits,
    admission: StoreAdmission,
) -> Result<CheckpointObjectBundle, SessionStoreError> {
    if let Some(existing) = state.checkpoints.get(&candidate.reference.checkpoint_id()) {
        return if existing.reference == candidate.reference {
            Ok(candidate.bundle)
        } else {
            Err(SessionStoreError::IdentityConflict)
        };
    }
    admit(
        StoreCapacityBound::Checkpoints,
        state.checkpoints.len().saturating_add(1),
        limits.checkpoints.get(),
        admission,
    )?;
    if candidate
        .base_root
        .is_some_and(|base| !state.objects.contains_key(&base))
    {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingBase,
        ));
    }
    if candidate
        .objects
        .windows(2)
        .any(|pair| pair[0].address() >= pair[1].address())
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::NoncanonicalCheckpointObjects,
        ));
    }
    let mut missing_objects = 0_usize;
    let mut missing_bytes = 0_usize;
    let supplied = candidate
        .objects
        .iter()
        .map(RuntimeObject::address)
        .collect::<BTreeSet<_>>();
    if !state.objects.contains_key(&candidate.reference.root())
        && !supplied.contains(&candidate.reference.root())
    {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingRoot,
        ));
    }
    for object in &candidate.objects {
        if object
            .children()
            .iter()
            .any(|child| !state.objects.contains_key(child) && !supplied.contains(child))
        {
            return Err(SessionStoreError::Corrupt(
                plakous_agent::internal::StoreIntegrityReason::MissingChild,
            ));
        }
        if let Some(existing) = state.objects.get(&object.address()) {
            if !same_object(existing, object) {
                return Err(SessionStoreError::corrupt());
            }
        } else {
            missing_objects = missing_objects
                .checked_add(1)
                .ok_or_else(|| capacity(StoreCapacityBound::Objects, state.objects.len(), 0))?;
            missing_bytes = missing_bytes
                .checked_add(object.retained_bytes())
                .ok_or_else(|| capacity(StoreCapacityBound::ObjectBytes, state.object_bytes, 0))?;
        }
    }
    let object_count = state
        .objects
        .len()
        .checked_add(missing_objects)
        .ok_or_else(|| capacity(StoreCapacityBound::Objects, state.objects.len(), 0))?;
    let object_bytes = state
        .object_bytes
        .checked_add(missing_bytes)
        .ok_or_else(|| capacity(StoreCapacityBound::ObjectBytes, state.object_bytes, 0))?;
    admit(
        StoreCapacityBound::Objects,
        object_count,
        limits.objects.get(),
        admission,
    )?;
    admit(
        StoreCapacityBound::ObjectBytes,
        object_bytes,
        limits.object_bytes.get(),
        admission,
    )?;
    for object in candidate.objects {
        state.objects.entry(object.address()).or_insert(object);
    }
    state.object_bytes = object_bytes;
    state.checkpoints.insert(
        candidate.reference.checkpoint_id(),
        CheckpointRecord {
            reference: candidate.reference,
            checkpoint: candidate.checkpoint,
        },
    );
    Ok(candidate.bundle)
}

/// Checkpoints of `selector` that a create, a fork or another branch names.
fn pinned_checkpoints(state: &StoreState, selector: SessionRef) -> BTreeSet<CheckpointId> {
    let mut pinned = state
        .creates
        .values()
        .map(|receipt| receipt.operation().initial().checkpoint_id())
        .chain(state.forks.values().flat_map(|receipt| {
            [
                receipt.source().checkpoint().checkpoint_id(),
                receipt.target().checkpoint().checkpoint_id(),
            ]
        }))
        .collect::<BTreeSet<_>>();
    for record in state.checkpoints.values() {
        let topology = record.checkpoint.manifest().topology();
        if let Some(fork) = topology.fork {
            pinned.insert(fork.checkpoint_id);
        }
        if (topology.session_id != selector.session_id()
            || topology.branch_id != selector.branch_id())
            && let Some(parent) = topology.parent_checkpoint_id
        {
            pinned.insert(parent);
        }
    }
    pinned
}

/// Whether retention reclaimed this checkpoint's commit receipt.
fn commit_history_pruned(state: &StoreState, checkpoint: CheckpointId) -> bool {
    if state.pruned.contains_key(&checkpoint) {
        return true;
    }
    state.checkpoints.get(&checkpoint).is_some_and(|record| {
        let topology = record.checkpoint.manifest().topology();
        topology.branch_revision > 0
            && state
                .horizons
                .get(&SessionRef::new(topology.session_id, topology.branch_id))
                .is_some_and(|horizon| topology.branch_revision <= horizon.get())
    })
}

/// Removes every object no retained checkpoint reaches; returns the removed
/// objects and their retained bytes.
fn sweep_unreachable(state: &mut StoreState) -> Result<(usize, usize), SessionStoreError> {
    let sweep = plan_object_sweep(state, &BTreeSet::new())?;
    let objects = sweep.objects.len();
    for address in sweep.objects {
        state.objects.remove(&address);
    }
    state.object_bytes = sweep.remaining_bytes;
    Ok((objects, sweep.bytes))
}

struct ObjectSweep {
    objects: Vec<RuntimeObjectAddress>,
    bytes: usize,
    remaining_bytes: usize,
}

fn plan_object_sweep(
    state: &StoreState,
    removed_checkpoints: &BTreeSet<CheckpointId>,
) -> Result<ObjectSweep, SessionStoreError> {
    let mut reachable = BTreeSet::new();
    let mut pending = state
        .checkpoints
        .iter()
        .filter(|(id, _)| !removed_checkpoints.contains(id))
        .map(|(_, record)| record.reference.root())
        .collect::<Vec<_>>();
    while let Some(address) = pending.pop() {
        if !reachable.insert(address) {
            continue;
        }
        let object = state
            .objects
            .get(&address)
            .ok_or(SessionStoreError::corrupt())?;
        pending.extend(object.children().iter().copied());
    }
    let swept = state
        .objects
        .iter()
        .filter_map(|(address, object)| {
            (!reachable.contains(address)).then_some((*address, object.retained_bytes()))
        })
        .collect::<Vec<_>>();
    let bytes = swept
        .iter()
        .try_fold(0_usize, |sum, (_, bytes)| sum.checked_add(*bytes))
        .ok_or_else(|| capacity(StoreCapacityBound::ObjectBytes, state.object_bytes, 0))?;
    let remaining_bytes = state
        .object_bytes
        .checked_sub(bytes)
        .ok_or(SessionStoreError::corrupt())?;
    Ok(ObjectSweep {
        objects: swept.into_iter().map(|(address, _)| address).collect(),
        bytes,
        remaining_bytes,
    })
}

fn retain_head_bundle(state: &mut StoreState, bundle: CheckpointObjectBundle) {
    state.hot_bundles.insert(bundle.checkpoint_id(), bundle);
    prune_hot_bundles(state);
}

fn prune_hot_bundles(state: &mut StoreState) {
    let heads = state
        .sessions
        .values()
        .filter(|session| !session.head.is_archived())
        .flat_map(|session| session.branches.values())
        .map(|branch| branch.head.checkpoint().checkpoint_id())
        .collect::<BTreeSet<_>>();
    state
        .hot_bundles
        .retain(|checkpoint_id, _| heads.contains(checkpoint_id));
}

fn same_object(left: &RuntimeObject, right: &RuntimeObject) -> bool {
    left.address() == right.address()
        && left.canonical_bytes() == right.canonical_bytes()
        && left.children() == right.children()
}

fn validate_genesis(
    candidate: &Candidate,
    request: &CreateSession,
) -> Result<(), SessionStoreError> {
    let checkpoint = &candidate.checkpoint;
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    if checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || request.initial().base_root().is_some()
        || topology.session_id != request.session().session_id()
        || topology.branch_id != request.session().branch_id()
        || topology.checkpoint_id != candidate.reference.checkpoint_id()
        || topology.parent_checkpoint_id.is_some()
        || topology.branch_revision != BranchRevision::INITIAL.get()
        || topology.fork.is_some()
        || topology.lease_fence != 1
        || manifest.durability() != RuntimeDurability::Disabled
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::GenesisCheckpointTopology,
        ));
    }
    Ok(())
}

fn validate_commit(
    candidate: &Candidate,
    request: &CommitCheckpoint,
    expected: &RuntimeCheckpoint,
) -> Result<(), SessionStoreError> {
    let checkpoint = &candidate.checkpoint;
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    let next = request
        .expected()
        .revision()
        .checked_next()
        .ok_or(SessionStoreError::Invalid(
            StoreInvalidReason::BranchRevisionExhausted,
        ))?;
    if topology.session_id != request.expected().session().session_id()
        || request.checkpoint().base_root() != Some(request.expected().checkpoint().root())
        || topology.branch_id != request.expected().session().branch_id()
        || topology.checkpoint_id != candidate.reference.checkpoint_id()
        || topology.parent_checkpoint_id != Some(request.expected().checkpoint().checkpoint_id())
        || topology.branch_revision != next.get()
        || topology.fork != expected.manifest().topology().fork
        || topology.lease_fence != request.lease().fence().get()
        || manifest.durability() != RuntimeDurability::Disabled
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointCommitTopology,
        ));
    }
    Ok(())
}

fn validate_fork(
    candidate: &Candidate,
    source: &RuntimeCheckpoint,
    request: &ForkBranch,
    target: SessionRef,
) -> Result<(), SessionStoreError> {
    let checkpoint = &candidate.checkpoint;
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    let expected_lineage = ForkLineage {
        session_id: request.source().session().session_id(),
        branch_id: request.source().session().branch_id(),
        checkpoint_id: request.source().checkpoint().checkpoint_id(),
        branch_revision: request.source().revision().get(),
    };
    if checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || request.target().base_root() != Some(request.source().checkpoint().root())
        || topology.session_id != target.session_id()
        || topology.branch_id != target.branch_id()
        || topology.checkpoint_id != candidate.reference.checkpoint_id()
        || topology.parent_checkpoint_id != Some(expected_lineage.checkpoint_id)
        || topology.branch_revision != BranchRevision::INITIAL.get()
        || topology.fork != Some(expected_lineage)
        || topology.lease_fence != 1
        || manifest.durability() != source.manifest().durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::ForkCheckpointSemantics,
        ));
    }
    validate_branch_manifest_transition(source, checkpoint, request.transition())
}

fn validate_fork_semantic_request(request: &ForkBranch) -> Result<(), SessionStoreError> {
    if request.semantic_request().domain() == DigestDomain::RequestPlan {
        Ok(())
    } else {
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::ForkRequestDigestDomain,
        ))
    }
}

fn validate_branch_manifest_transition(
    source: &RuntimeCheckpoint,
    target: &RuntimeCheckpoint,
    mode: BranchTransitionMode,
) -> Result<(), SessionStoreError> {
    let source_manifest = source.manifest();
    let target_manifest = target.manifest();
    let transition = match mode {
        BranchTransitionMode::OrdinaryFork => RuntimeContextTransitionKind::OrdinaryFork,
        BranchTransitionMode::Rebind => RuntimeContextTransitionKind::Rebind,
    };
    target_manifest
        .canonical()
        .context()
        .validate_branch_transition_from(
            source_manifest.canonical().context(),
            source_manifest.topology().checkpoint_id,
            transition,
        )
        .map_err(|_| {
            SessionStoreError::Invalid(StoreInvalidReason::InvalidBranchContextTransition)
        })?;
    if source_manifest.lifecycle() != &RuntimeLifecycle::Quiescent
        || target_manifest.lifecycle() != &RuntimeLifecycle::Quiescent
        || target_manifest.canonical().history() != source_manifest.canonical().history()
        || target_manifest.canonical().working() != source_manifest.canonical().working()
        || target_manifest.ledger() != source_manifest.ledger()
        || target_manifest.family_artifact().is_some()
        || target_manifest.durability() != source_manifest.durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::UnauthorizedBranchManifestTransition,
        ));
    }
    let same_configuration = target_manifest.harness_plan_component()
        == source_manifest.harness_plan_component()
        && target_manifest.canonical().stable_prefix()
            == source_manifest.canonical().stable_prefix()
        && target_manifest.binding() == source_manifest.binding()
        && target_manifest.tools() == source_manifest.tools();
    match mode {
        BranchTransitionMode::OrdinaryFork => {
            if !same_configuration || target_manifest.provenance() != source_manifest.provenance() {
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::OrdinaryForkChangedSemantics,
                ));
            }
        }
        BranchTransitionMode::Rebind => {
            if same_configuration {
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::RebindConfigurationUnchanged,
                ));
            }
        }
    }
    Ok(())
}

fn validate_materialized_fork_receipt(
    receipt: &ForkReceipt,
    source: &StoredCheckpoint,
    target: &StoredCheckpoint,
) -> Result<(), SessionStoreError> {
    let operation = receipt.operation();
    let source_checkpoint = source.checkpoint().as_ref();
    let target_checkpoint = target.checkpoint().as_ref();
    let source_topology = source_checkpoint.manifest().topology();
    let target_topology = target_checkpoint.manifest().topology();
    let previous_session = receipt.previous_session();
    let session = receipt.session();
    let expected_lineage = ForkLineage {
        session_id: operation.source().session().session_id(),
        branch_id: operation.source().session().branch_id(),
        checkpoint_id: operation.source().checkpoint().checkpoint_id(),
        branch_revision: operation.source().revision().get(),
    };
    if receipt.source() != operation.source()
        || receipt.target().session() != operation.target()
        || receipt.target().revision() != BranchRevision::INITIAL
        || receipt.target().checkpoint() != operation.checkpoint()
        || receipt.lease().session() != operation.target()
        || receipt.lease().fence() != NonZeroU64::MIN
        || previous_session.session_id() != operation.target().session_id()
        || previous_session.revision() != operation.expected_session_revision()
        || previous_session.revision().checked_next() != Some(session.revision())
        || session.session_id() != previous_session.session_id()
        || session.initial_branch() != previous_session.initial_branch()
        || session.created_at() != previous_session.created_at()
        || session.durability() != previous_session.durability()
        || session.is_archived()
        || previous_session.is_archived()
        || source.reference() != operation.source().checkpoint()
        || target.reference() != operation.checkpoint()
        || target_checkpoint.checkpoint_commit_identity() != operation.checkpoint_commit_identity()
        || source_checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || target_checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || source_topology.session_id != operation.source().session().session_id()
        || source_topology.branch_id != operation.source().session().branch_id()
        || source_topology.checkpoint_id != operation.source().checkpoint().checkpoint_id()
        || source_topology.branch_revision != operation.source().revision().get()
        || target_topology.session_id != operation.target().session_id()
        || target_topology.branch_id != operation.target().branch_id()
        || target_topology.checkpoint_id != operation.checkpoint().checkpoint_id()
        || target_topology.parent_checkpoint_id != Some(expected_lineage.checkpoint_id)
        || target_topology.branch_revision != BranchRevision::INITIAL.get()
        || target_topology.fork != Some(expected_lineage)
        || target_topology.lease_fence != receipt.lease().fence().get()
        || source_checkpoint.manifest().durability() != session.durability()
        || target_checkpoint.manifest().durability() != session.durability()
    {
        return Err(SessionStoreError::corrupt());
    }
    validate_branch_manifest_transition(
        source_checkpoint,
        target_checkpoint,
        operation.transition(),
    )
    .map_err(|_| SessionStoreError::corrupt())
}

fn create_matches(
    receipt: &CreateReceipt,
    request: &CreateSession,
    reference: CheckpointObjectRef,
) -> bool {
    let operation = receipt.operation();
    operation.session() == request.session()
        && operation.created_at() == request.created_at()
        && operation.owner() == request.owner()
        && operation.lease_seconds() == request.lease_seconds()
        && operation.initial() == reference
}

fn commit_matches(
    committed: &CommittedCheckpoint,
    request: &CommitCheckpoint,
    reference: CheckpointObjectRef,
) -> bool {
    committed.operation().lease() == request.lease()
        && committed.operation().expected() == request.expected()
        && committed.operation().checkpoint() == reference
}

fn fork_matches(receipt: &ForkReceipt, request: &ForkBranch, candidate: &Candidate) -> bool {
    let operation = receipt.operation();
    operation.workflow_id() == request.workflow_id()
        && operation.semantic_request() == request.semantic_request()
        && operation.expected_session_revision() == request.expected_session_revision()
        && operation.source() == request.source()
        && operation.target().branch_id() == request.target_branch()
        && operation.transition() == request.transition()
        && operation.checkpoint() == candidate.reference
        && operation.checkpoint_commit_identity()
            == candidate.checkpoint.checkpoint_commit_identity()
        && candidate.base_root == Some(operation.source().checkpoint().root())
}

fn recover_fork_matches(receipt: &ForkReceipt, request: &RecoverFork) -> bool {
    let operation = receipt.operation();
    operation.workflow_id() == request.workflow_id()
        && operation.source() == request.source()
        && operation.target().branch_id() == request.target_branch()
        && operation.transition() == request.transition()
        && operation.semantic_request() == request.semantic_request()
}

fn stored_checkpoint(
    state: &StoreState,
    reference: CheckpointObjectRef,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let record = state
        .checkpoints
        .get(&reference.checkpoint_id())
        .ok_or(SessionStoreError::corrupt())?;
    if record.reference != reference {
        return Err(SessionStoreError::corrupt());
    }
    if let Some(bundle) = state.hot_bundles.get(&reference.checkpoint_id()) {
        return StoredCheckpoint::from_store(record.reference, bundle.clone())
            .map_err(|_| SessionStoreError::corrupt());
    }
    cold_checkpoint(state, reference)
}

fn cold_checkpoint(
    state: &StoreState,
    reference: CheckpointObjectRef,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let mut reachable = BTreeSet::new();
    let mut pending = vec![reference.root()];
    while let Some(address) = pending.pop() {
        if !reachable.insert(address) {
            continue;
        }
        let object = state
            .objects
            .get(&address)
            .ok_or(SessionStoreError::corrupt())?;
        pending.extend(object.children().iter().copied());
    }
    let objects = reachable
        .into_iter()
        .map(|address| {
            state
                .objects
                .get(&address)
                .cloned()
                .ok_or(SessionStoreError::corrupt())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let bundle = CheckpointObjectBundle::from_store(
        reference.checkpoint_id(),
        reference.integrity(),
        reference.root(),
        objects,
    )
    .map_err(|_| SessionStoreError::corrupt())?;
    StoredCheckpoint::from_store(reference, bundle).map_err(|_| SessionStoreError::corrupt())
}

fn branch(state: &StoreState, selector: SessionRef) -> Result<&BranchRecord, SessionStoreError> {
    let session = state
        .sessions
        .get(&selector.session_id())
        .ok_or(SessionStoreError::NotFound)?;
    if session.head.is_archived() {
        return Err(SessionStoreError::Archived);
    }
    session
        .branches
        .get(&selector.branch_id())
        .ok_or(SessionStoreError::NotFound)
}

fn mutable_branch(
    state: &mut StoreState,
    selector: SessionRef,
) -> Result<&mut BranchRecord, SessionStoreError> {
    let session = state
        .sessions
        .get_mut(&selector.session_id())
        .ok_or(SessionStoreError::NotFound)?;
    if session.head.is_archived() {
        return Err(SessionStoreError::Archived);
    }
    session
        .branches
        .get_mut(&selector.branch_id())
        .ok_or(SessionStoreError::NotFound)
}

fn require_lease(
    branch: &BranchRecord,
    lease: &BranchLease,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    require_lease_domain(lease, now)?;
    let current = branch
        .active_lease
        .as_ref()
        .ok_or(SessionStoreError::FenceSuperseded)?;
    if current != lease {
        return Err(SessionStoreError::FenceSuperseded);
    }
    if expired(current, now)? {
        return Err(SessionStoreError::LeaseExpired);
    }
    Ok(())
}

fn require_open(state: &StoreState) -> Result<(), SessionStoreError> {
    if state.shutdown_report.is_some() {
        Err(SessionStoreError::Shutdown)
    } else {
        Ok(())
    }
}

fn sample_clock(
    clock: &dyn LeaseClock,
    domain: LeaseDomain,
) -> Result<LeaseInstant, SessionStoreError> {
    let now = clock.now()?;
    if now.domain() != domain {
        return Err(SessionStoreError::LeaseDomainMismatch);
    }
    Ok(now)
}

fn require_lease_domain(lease: &BranchLease, now: LeaseInstant) -> Result<(), SessionStoreError> {
    if lease.expires_at().domain().epoch() != now.domain().epoch() {
        return Err(SessionStoreError::FenceSuperseded);
    }
    lease.expires_at().same_domain_cmp(now)?;
    Ok(())
}

fn expiry(now: LeaseInstant, seconds: NonZeroU64) -> Result<LeaseInstant, SessionStoreError> {
    now.checked_add_seconds(seconds.get())
}

fn expired(lease: &BranchLease, now: LeaseInstant) -> Result<bool, SessionStoreError> {
    if lease.expires_at().domain().epoch() != now.domain().epoch() {
        return Ok(true);
    }
    Ok(now.same_domain_cmp(lease.expires_at())? != std::cmp::Ordering::Less)
}

fn memory_report() -> DurabilityReport {
    DurabilityReport::from_store(RuntimeDurability::Disabled, 0, None)
}

fn capacity(bound: StoreCapacityBound, current: usize, max: usize) -> SessionStoreError {
    SessionStoreError::capacity(bound, count(current), count(max))
}

/// Holds an aggregate bound to `admission`'s share of `limit`.
fn admit(
    bound: StoreCapacityBound,
    attempted: usize,
    limit: usize,
    admission: StoreAdmission,
) -> Result<(), SessionStoreError> {
    let (attempted, limit) = (count(attempted), count(limit));
    if attempted > admission.threshold(limit) {
        return Err(SessionStoreError::admission_capacity(
            bound, attempted, limit, admission,
        ));
    }
    Ok(())
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

const fn nonzero(value: usize) -> NonZeroUsize {
    match NonZeroUsize::new(value) {
        Some(value) => value,
        None => panic!("store default bounds are nonzero"),
    }
}

#[cfg(test)]
#[path = "../tests/fixture.rs"]
mod integrity_fixture;

#[cfg(test)]
mod integrity_tests {
    use super::integrity_fixture as fixture;
    use super::*;

    struct FixedClock;

    impl LeaseClock for FixedClock {
        fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
            LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 100, 0)
        }
    }

    fn prune_fixture() -> fixture::TestResult<(MemorySessionStore, PruneBranch)> {
        let store =
            MemorySessionStore::with_clock(MemoryStoreLimits::default(), Arc::new(FixedClock))?;
        let selector = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
        let mut prior = fixture::bundle(
            &fixture::checkpoint(
                fixture::topology(
                    selector.session_id(),
                    selector.branch_id(),
                    fixture::id(100)?,
                    None,
                    0,
                    None,
                    1,
                )?,
                1,
                RuntimeDurability::Disabled,
            )?,
            None,
        )?;
        let created = store.create_sync(&CreateSession::new(
            selector,
            plakous_agent::internal::Timestamp::new(50, 0)?,
            fixture::id(3)?,
            NonZeroU64::new(1_000).ok_or("nonzero lease")?,
            prior.clone(),
        ))?;
        let mut head = created.branch();
        for revision in 1..=4 {
            let next = fixture::checkpoint(
                fixture::topology(
                    selector.session_id(),
                    selector.branch_id(),
                    fixture::id(100 + u128::from(revision))?,
                    Some(prior.checkpoint_id()),
                    revision,
                    None,
                    1,
                )?,
                2,
                RuntimeDurability::Disabled,
            )?;
            prior = fixture::bundle(&next, Some(&prior))?;
            head = store
                .commit_sync(&CommitCheckpoint::new(
                    created.lease().clone(),
                    head,
                    prior.clone(),
                ))?
                .head();
        }
        Ok((
            store,
            PruneBranch::new(created.lease().clone(), head, BTreeSet::new(), 1),
        ))
    }

    #[test]
    fn memory_prune_preserves_retained_map_allocation() -> fixture::TestResult {
        let (store, request) = prune_fixture()?;
        let session = request.expected().session().session_id();
        let before = std::ptr::from_ref(store.lock()?.sessions.get(&session).ok_or("session")?);
        let receipt = store.prune_sync(&request)?;
        assert_eq!(receipt.pruned_checkpoints(), 2);
        let state = store.lock()?;
        assert_eq!(
            before,
            std::ptr::from_ref(state.sessions.get(&session).ok_or("session")?)
        );
        assert!(!state.checkpoints.contains_key(&fixture::id(101)?));
        assert!(state.checkpoints.contains_key(&fixture::id(104)?));
        Ok(())
    }

    #[test]
    fn memory_prune_failed_sweep_leaves_state_unchanged() -> fixture::TestResult {
        for corrupt_graph in [true, false] {
            let (store, request) = prune_fixture()?;
            {
                let mut state = store.lock()?;
                if corrupt_graph {
                    state
                        .objects
                        .remove(&request.expected().checkpoint().root());
                } else {
                    state.object_bytes = 0;
                }
            }
            let before = store.lock()?.clone();
            assert_eq!(
                store.prune_sync(&request),
                Err(SessionStoreError::corrupt())
            );
            let after = store.lock()?;
            assert_eq!(
                after.checkpoints.keys().collect::<Vec<_>>(),
                before.checkpoints.keys().collect::<Vec<_>>()
            );
            assert_eq!(
                after.commits.keys().collect::<Vec<_>>(),
                before.commits.keys().collect::<Vec<_>>()
            );
            assert_eq!(
                after.hot_bundles.keys().collect::<Vec<_>>(),
                before.hot_bundles.keys().collect::<Vec<_>>()
            );
            assert_eq!(after.objects, before.objects);
            assert_eq!(after.object_bytes, before.object_bytes);
            assert_eq!(after.pruned, before.pruned);
            assert_eq!(after.horizons, before.horizons);
        }
        Ok(())
    }

    #[test]
    fn missing_publication_base_or_root_leaves_memory_state_unchanged() -> fixture::TestResult {
        use plakous_agent::internal::StoreIntegrityReason;
        for reason in [
            StoreIntegrityReason::MissingBase,
            StoreIntegrityReason::MissingRoot,
        ] {
            let (base, next) = fixture::integrity_publication_pair()?;
            let mut state = StoreState::default();
            install_candidate(
                &mut state,
                publication_candidate(&base)?,
                MemoryStoreLimits::default(),
                StoreAdmission::Ordinary,
            )?;
            let before = (
                state.checkpoints.len(),
                state.objects.len(),
                state.object_bytes,
            );
            let mut candidate = publication_candidate(&next)?;
            if reason == StoreIntegrityReason::MissingBase {
                candidate.base_root = Some(candidate.reference.root());
            } else {
                candidate
                    .objects
                    .retain(|object| object.address() != candidate.reference.root());
            }
            assert_eq!(
                install_candidate(
                    &mut state,
                    candidate,
                    MemoryStoreLimits::default(),
                    StoreAdmission::Ordinary
                )
                .err(),
                Some(SessionStoreError::Corrupt(reason)),
            );
            assert_eq!(
                (
                    state.checkpoints.len(),
                    state.objects.len(),
                    state.object_bytes
                ),
                before
            );
            assert_eq!(
                state
                    .checkpoints
                    .get(&base.checkpoint_id())
                    .ok_or("base checkpoint")?
                    .reference,
                bundle_reference(&base)
            );
            assert!(!state.checkpoints.contains_key(&next.checkpoint_id()));
        }
        Ok(())
    }
}
