//! Process-memory session store.

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

use super::lease_clock::SystemLeaseClock;
use super::memory_feed::{self, MemoryFeedLimits, SessionFeed};

#[cfg(test)]
mod tests;

mod candidates;
use candidates::{
    bundle_reference, commit_history_pruned, install_candidate, pinned_checkpoints,
    plan_object_sweep, prune_hot_bundles, publication_candidate, retain_head_bundle,
    sweep_unreachable, validated_checkpoint,
};
pub(crate) mod receipts;
use receipts::{
    commit_matches, create_matches, fork_matches, recover_fork_matches, validate_commit,
    validate_fork, validate_fork_semantic_request, validate_genesis,
    validate_materialized_fork_receipt,
};
mod lines;
mod reads;
use reads::{branch, stored_checkpoint};

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

    #[cfg(feature = "sqlite")]
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
        let checkpoint = validated_checkpoint(request.initial())?;
        validate_genesis(&checkpoint, request)?;
        let checkpoint_commit_identity = checkpoint.checkpoint_commit_identity();
        if state.deletes.contains_key(&request.session().session_id()) {
            return Err(SessionStoreError::IdentityConflict);
        }
        if let Some(receipt) = state.creates.get(&request.session().session_id()) {
            return if create_matches(receipt, request, reference, checkpoint_commit_identity) {
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
        let topology = candidate.checkpoint.manifest().topology();
        let now = self.lease_now()?;
        let line_change = lines::plan_genesis(&state, request, topology, now)?;
        let expires_at = expiry(now, request.lease_seconds())?;
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
                line: topology.line,
                generation: topology.line_generation,
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
        lines::apply(&mut state, line_change);
        retain_head_bundle(&mut state, hot_bundle);
        Ok(receipt)
    }

    fn recover_create_sync(
        &self,
        request: &CreateSession,
    ) -> Result<CreateReceipt, SessionStoreError> {
        let reference = bundle_reference(request.initial());
        let checkpoint_commit_identity =
            validated_checkpoint(request.initial())?.checkpoint_commit_identity();
        let state = self.lock()?;
        match state.creates.get(&request.session().session_id()) {
            Some(receipt) => {
                if create_matches(receipt, request, reference, checkpoint_commit_identity) {
                    Ok(receipt.clone())
                } else {
                    Err(SessionStoreError::IdentityConflict)
                }
            }
            None => Err(SessionStoreError::NotFound),
        }
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

    /// Validates every lease before releasing any, under one hold of the state lock.
    fn release_many_sync(&self, leases: &[BranchLease]) -> Result<(), SessionStoreError> {
        let mut guard = self.lock()?;
        let state = &mut *guard;
        require_open(state)?;
        let now = self.lease_now()?;
        let mut releasing: Vec<&BranchLease> = Vec::new();
        let mut changes = Vec::new();
        for lease in leases {
            require_lease_domain(lease, now)?;
            let branch = mutable_branch(state, lease.session())?;
            if (branch.released_lease.as_ref() == Some(lease) && branch.active_lease.is_none())
                || releasing.contains(&lease)
            {
                continue;
            }
            if branch.active_lease.as_ref() != Some(lease) {
                return Err(SessionStoreError::FenceSuperseded);
            }
            changes.push((
                lease.session(),
                FeedChange::LeaseReleased {
                    head: branch.head.checkpoint().checkpoint_id(),
                    fence: lease.fence().get(),
                },
            ));
            releasing.push(lease);
        }
        let records = memory_feed::prepare_changes(&state.feeds, changes)?;
        for lease in releasing {
            let branch = mutable_branch(state, lease.session())?;
            branch.active_lease = None;
            branch.previous_renewal = None;
            branch.released_lease = Some(lease.clone());
        }
        for record in records {
            memory_feed::append(&mut state.feeds, record);
        }
        Ok(())
    }

    fn commit_sync(&self, request: &CommitCheckpoint) -> Result<CommitReceipt, SessionStoreError> {
        let reference = bundle_reference(request.checkpoint());
        let checkpoint_id = reference.checkpoint_id();
        let mut state = self.lock()?;
        require_open(&state)?;
        let checkpoint_commit_identity =
            validated_checkpoint(request.checkpoint())?.checkpoint_commit_identity();
        if let Some(committed) = state.commits.get(&checkpoint_id) {
            return if commit_matches(committed, request, reference, checkpoint_commit_identity) {
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
        let topology = candidate.checkpoint.manifest().topology();
        let (line, line_generation) = (topology.line, topology.line_generation);
        lines::require_head(&state, request.expected().session(), line, line_generation)?;
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
        lines::consume(
            &mut state,
            line,
            line_generation,
            request.consumed_line_terminals(),
        );
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
        let (line, line_generation) = lines::branch_line(&state, selector)?;
        lines::require_head(&state, selector, line, line_generation)?;
        let checkpoint = Arc::clone(
            &state
                .checkpoints
                .get(&head.checkpoint().checkpoint_id())
                .ok_or(SessionStoreError::corrupt())?
                .checkpoint,
        );
        if checkpoint.kind() != RuntimeCheckpointKind::Quiescent && !request.admits_working_head() {
            return Err(SessionStoreError::Invalid(
                StoreInvalidReason::PruneHeadNotQuiescent,
            ));
        }
        let stored = state.horizons.get(&selector).copied();
        let unchanged =
            PruneReceipt::from_store(head, stored.unwrap_or(BranchRevision::INITIAL), 0);
        let Some(horizon) = request.horizon_revision() else {
            return Ok(unchanged);
        };
        // A request at or below the horizon already recorded must not move it
        // backward: the earlier prune kept checkpoints between the two.
        if stored.is_some_and(|revision| horizon <= revision.get()) {
            return Ok(unchanged);
        }
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
        let topology = candidate.checkpoint.manifest().topology();
        let (line_change, new_head) = lines::plan_step(
            &state,
            &lines::Step {
                target,
                line: topology.line,
                generation: topology.line_generation,
                source: Some((
                    request.source().session(),
                    request.source().checkpoint().checkpoint_id(),
                )),
                consumed: request.consumed_line_terminals(),
                origin: &plakous_agent::internal::LineOrigin::Created,
                invalid: StoreInvalidReason::ForkCheckpointSemantics,
            },
        )?;
        let (line, line_generation) = (topology.line, topology.line_generation);
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
                line,
                new_head,
                generation: line_generation,
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
        lines::apply(&mut state, line_change);
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
        lines::delete_session(&mut staged, request.session_id());
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

    fn release_many(&self, leases: Vec<BranchLease>) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            self.release_many_sync(&leases)?;
            let sessions = leases
                .iter()
                .map(|lease| lease.session().session_id())
                .collect::<BTreeSet<_>>();
            for session_id in sessions {
                self.trim_feed(session_id);
            }
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

    fn load_line(
        &self,
        line: plakous_agent::internal::LineId,
    ) -> StoreFuture<'_, plakous_agent::internal::LineRecord> {
        Box::pin(async move { lines::load(&*self.lock()?, line) })
    }

    fn line_job_terminals(
        &self,
        line: plakous_agent::internal::LineId,
    ) -> StoreFuture<'_, Vec<plakous_agent::internal::LineJobTerminal>> {
        Box::pin(async move { lines::terminals(&*self.lock()?, line) })
    }

    fn record_line_job_terminals(
        &self,
        request: plakous_agent::internal::RecordLineJobTerminals,
    ) -> StoreFuture<'_, plakous_agent::internal::LineTerminalsReceipt> {
        Box::pin(async move {
            let now = self.lease_now()?;
            let mut state = self.lock()?;
            require_open(&state)?;
            let receipt = lines::record(&mut state, &request, now)?;
            drop(state);
            self.trim_feed(request.lease().session().session_id());
            Ok(receipt)
        })
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
    lines: lines::MemoryLines,
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

struct ObjectSweep {
    objects: Vec<RuntimeObjectAddress>,
    bytes: usize,
    remaining_bytes: usize,
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

pub(crate) fn sample_clock(
    clock: &dyn LeaseClock,
    domain: LeaseDomain,
) -> Result<LeaseInstant, SessionStoreError> {
    let now = clock.now()?;
    if now.domain() != domain {
        return Err(SessionStoreError::LeaseDomainMismatch);
    }
    Ok(now)
}

pub(crate) fn require_lease_domain(
    lease: &BranchLease,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    if lease.expires_at().domain().epoch() != now.domain().epoch() {
        return Err(SessionStoreError::FenceSuperseded);
    }
    lease.expires_at().same_domain_cmp(now)?;
    Ok(())
}

pub(crate) fn expiry(
    now: LeaseInstant,
    seconds: NonZeroU64,
) -> Result<LeaseInstant, SessionStoreError> {
    now.checked_add_seconds(seconds.get())
}

pub(crate) fn expired(lease: &BranchLease, now: LeaseInstant) -> Result<bool, SessionStoreError> {
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
