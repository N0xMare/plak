//! Semantic session-store contract owned by the agent runtime.
//!
//! Backends publish immutable [`RuntimeCheckpoint`] object bundles under exact
//! branch CAS and lease fencing. They never interpret canonical history,
//! operation-ledger, family-artifact, event, transcript, or presentation state.

mod store;
pub use store::SessionStore;
pub use store::StoreFuture;
mod lease;
pub use lease::AcquireBranch;
pub use lease::BranchLease;
mod create;
pub use create::CreateOperation;
pub use create::CreateReceipt;
pub use create::CreateSession;
mod fork;
pub use fork::BranchTransitionMode;
pub use fork::ForkBranch;
pub use fork::ForkOperation;
pub use fork::ForkReceipt;
pub use fork::RecoverFork;
mod commit;
pub use commit::CommitCheckpoint;
pub use commit::CommitOperation;
pub use commit::CommitReceipt;
pub use commit::CommittedCheckpoint;
mod errors;
pub use errors::SessionStoreCapacity;
pub use errors::SessionStoreError;
pub use errors::StoreAdmission;
pub use errors::StoreBoundUsage;
pub use errors::StoreCapacityBound;
pub use errors::StoreIntegrityReason;
pub use errors::StoreInvalidReason;
pub use errors::StoreUsage;

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::num::{NonZeroU64, NonZeroUsize};
use std::pin::Pin;
use std::sync::Arc;

use crate::lease::LeaseInstant;
use plakous_model::{BranchId, CheckpointId, Digest, RunId, SessionId, Timestamp};
use thiserror::Error;

use crate::feed::{FeedFact, FeedPage, FeedSeq, FeedTail};
use crate::{
    CheckpointBundleError, CheckpointObjectBundle, RuntimeCheckpoint, RuntimeDurability,
    RuntimeObjectAddress,
};

/// Fixed byte width of the one canonical session-workflow root identity.
/// The one canonical caller-stable workflow/root identity used by model calls,
/// manifests, and session-store receipts.
pub use plakous_model::SessionWorkflowRootId as SessionWorkflowId;

/// Exact session and branch selector.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionRef {
    session_id: SessionId,
    branch_id: BranchId,
}

impl SessionRef {
    /// Creates an exact branch selector.
    #[must_use]
    pub const fn new(session_id: SessionId, branch_id: BranchId) -> Self {
        Self {
            session_id,
            branch_id,
        }
    }

    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the branch identity.
    #[must_use]
    pub const fn branch_id(self) -> BranchId {
        self.branch_id
    }
}

impl fmt::Display for SessionRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.session_id, self.branch_id)
    }
}

/// Monotonic session-topology revision.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionRevision(u64);

impl SessionRevision {
    /// Initial session-topology revision.
    pub const INITIAL: Self = Self(0);

    /// Reconstructs a backend-owned revision.
    #[must_use]
    pub const fn from_store(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next revision, or `None` on exhaustion.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// Monotonic revision of one independent branch head.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BranchRevision(u64);

impl BranchRevision {
    /// Initial branch revision containing its genesis checkpoint.
    pub const INITIAL: Self = Self(0);

    /// Reconstructs a backend-owned revision.
    #[must_use]
    pub const fn from_store(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next revision, or `None` on exhaustion.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// Stable semantic reference to one immutable checkpoint object graph.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CheckpointObjectRef {
    checkpoint_id: CheckpointId,
    root: RuntimeObjectAddress,
    integrity: Digest,
}

impl CheckpointObjectRef {
    /// Creates a reference validated by a store backend.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        checkpoint_id: CheckpointId,
        root: RuntimeObjectAddress,
        integrity: Digest,
    ) -> Self {
        Self {
            checkpoint_id,
            root,
            integrity,
        }
    }

    /// Returns the commit operation identity.
    #[must_use]
    pub const fn checkpoint_id(self) -> CheckpointId {
        self.checkpoint_id
    }

    /// Returns the exact content-addressed root.
    #[must_use]
    pub const fn root(self) -> RuntimeObjectAddress {
        self.root
    }

    /// Returns checkpoint-integrity evidence.
    #[must_use]
    pub const fn integrity(self) -> Digest {
        self.integrity
    }
}

/// One exact immutable branch head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BranchHead {
    session: SessionRef,
    revision: BranchRevision,
    checkpoint: CheckpointObjectRef,
}

impl BranchHead {
    /// Creates a backend-returned exact branch head.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        session: SessionRef,
        revision: BranchRevision,
        checkpoint: CheckpointObjectRef,
    ) -> Self {
        Self {
            session,
            revision,
            checkpoint,
        }
    }

    /// Returns the selected branch.
    #[must_use]
    pub const fn session(self) -> SessionRef {
        self.session
    }

    /// Returns its exact independent revision.
    #[must_use]
    pub const fn revision(self) -> BranchRevision {
        self.revision
    }

    /// Returns its exact checkpoint object root.
    #[must_use]
    pub const fn checkpoint(self) -> CheckpointObjectRef {
        self.checkpoint
    }
}

/// Current session metadata and topology revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionHead {
    session_id: SessionId,
    initial_branch: BranchId,
    revision: SessionRevision,
    created_at: Timestamp,
    durability: RuntimeDurability,
    archived: bool,
}

impl SessionHead {
    /// Creates backend-returned session metadata.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        session_id: SessionId,
        initial_branch: BranchId,
        revision: SessionRevision,
        created_at: Timestamp,
        durability: RuntimeDurability,
        archived: bool,
    ) -> Self {
        Self {
            session_id,
            initial_branch,
            revision,
            created_at,
            durability,
            archived,
        }
    }

    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the initial branch identity.
    #[must_use]
    pub const fn initial_branch(self) -> BranchId {
        self.initial_branch
    }

    /// Returns the exact topology/archive revision.
    #[must_use]
    pub const fn revision(self) -> SessionRevision {
        self.revision
    }

    /// Returns the creation time.
    #[must_use]
    pub const fn created_at(self) -> Timestamp {
        self.created_at
    }

    /// Returns the selected durability contract.
    #[must_use]
    pub const fn durability(self) -> RuntimeDurability {
        self.durability
    }

    /// Reports whether mutation and ordinary load are closed.
    #[must_use]
    pub const fn is_archived(self) -> bool {
        self.archived
    }
}

/// Materialized immutable checkpoint returned by a backend.
#[derive(Clone, Debug)]
pub struct StoredCheckpoint {
    reference: CheckpointObjectRef,
    checkpoint: Arc<RuntimeCheckpoint>,
    bundle: CheckpointObjectBundle,
}

impl StoredCheckpoint {
    /// Creates a backend-returned exact checkpoint.
    #[doc(hidden)]
    ///
    /// # Errors
    ///
    /// Returns an error if the verified bundle does not exactly match the
    /// backend's checkpoint reference.
    pub fn from_store(
        reference: CheckpointObjectRef,
        bundle: CheckpointObjectBundle,
    ) -> Result<Self, CheckpointBundleError> {
        let checkpoint = bundle.validated_checkpoint()?;
        if bundle.checkpoint_id() != reference.checkpoint_id()
            || bundle.checkpoint_integrity() != reference.integrity()
            || bundle.root() != reference.root()
            || checkpoint.manifest().topology().checkpoint_id != reference.checkpoint_id()
            || checkpoint.integrity() != reference.integrity()
        {
            return Err(CheckpointBundleError::SemanticDisagreement);
        }
        Ok(Self {
            reference,
            checkpoint,
            bundle,
        })
    }

    /// Returns its exact object reference.
    #[must_use]
    pub const fn reference(&self) -> CheckpointObjectRef {
        self.reference
    }

    /// Returns the sole semantic checkpoint authority.
    #[must_use]
    pub fn checkpoint(&self) -> &Arc<RuntimeCheckpoint> {
        &self.checkpoint
    }

    /// Returns the verified object graph retained by the loading backend.
    #[doc(hidden)]
    #[must_use]
    pub const fn bundle(&self) -> &CheckpointObjectBundle {
        &self.bundle
    }
}

impl PartialEq for StoredCheckpoint {
    fn eq(&self, other: &Self) -> bool {
        self.reference == other.reference && self.checkpoint == other.checkpoint
    }
}

/// Exact materialized view of one current branch.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedBranch {
    session: SessionHead,
    head: BranchHead,
    checkpoint: StoredCheckpoint,
    horizon: Option<BranchRevision>,
}

impl LoadedBranch {
    /// Creates a backend-returned branch view.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        session: SessionHead,
        head: BranchHead,
        checkpoint: StoredCheckpoint,
    ) -> Self {
        Self {
            session,
            head,
            checkpoint,
            horizon: None,
        }
    }

    /// Attaches the branch history horizon the backend has persisted.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_horizon(mut self, horizon: BranchRevision) -> Self {
        self.horizon = Some(horizon);
        self
    }

    /// Returns current session metadata.
    #[must_use]
    pub const fn session(&self) -> SessionHead {
        self.session
    }

    /// Returns the exact current branch head.
    #[must_use]
    pub const fn head(&self) -> BranchHead {
        self.head
    }

    /// Returns the materialized head checkpoint.
    #[must_use]
    pub const fn checkpoint(&self) -> &StoredCheckpoint {
        &self.checkpoint
    }

    /// Returns the branch history horizon of its last acknowledged prune, or
    /// `None` when retention never ran on this branch.
    #[must_use]
    pub const fn horizon(&self) -> Option<BranchRevision> {
        self.horizon
    }
}

/// Bounded session-list query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionQuery {
    after: Option<SessionId>,
    limit: NonZeroUsize,
    include_archived: bool,
}

impl SessionQuery {
    /// Creates a bounded query excluding archived sessions.
    #[must_use]
    pub const fn new(limit: NonZeroUsize) -> Self {
        Self {
            after: None,
            limit,
            include_archived: false,
        }
    }

    /// Starts strictly after one session identity.
    #[must_use]
    pub const fn after(mut self, session_id: SessionId) -> Self {
        self.after = Some(session_id);
        self
    }

    /// Selects whether archived sessions are visible.
    #[must_use]
    pub const fn include_archived(mut self, include: bool) -> Self {
        self.include_archived = include;
        self
    }

    /// Returns the exclusive cursor.
    #[must_use]
    pub const fn after_session(self) -> Option<SessionId> {
        self.after
    }

    /// Returns the requested bound.
    #[must_use]
    pub const fn limit(self) -> NonZeroUsize {
        self.limit
    }

    /// Returns whether archived sessions are selected.
    #[must_use]
    pub const fn includes_archived(self) -> bool {
        self.include_archived
    }
}

/// Stable session summary returned by listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    head: SessionHead,
    branch_count: u64,
}

impl SessionSummary {
    /// Creates a backend-returned summary.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(head: SessionHead, branch_count: u64) -> Self {
        Self { head, branch_count }
    }

    /// Returns current session metadata.
    #[must_use]
    pub const fn head(self) -> SessionHead {
        self.head
    }

    /// Returns the number of independent branches.
    #[must_use]
    pub const fn branch_count(self) -> u64 {
        self.branch_count
    }
}

/// One bounded listing page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page<T> {
    items: Vec<T>,
    next_after: Option<SessionId>,
}

impl<T> Page<T> {
    /// Creates a backend-returned page.
    #[doc(hidden)]
    #[must_use]
    pub fn from_store(items: Vec<T>, next_after: Option<SessionId>) -> Self {
        Self { items, next_after }
    }

    /// Returns the ordered page items.
    #[must_use]
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Returns the next exclusive cursor.
    #[must_use]
    pub const fn next_after(&self) -> Option<SessionId> {
        self.next_after
    }
}

/// Expected-revision archive request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveSession {
    session_id: SessionId,
    expected_revision: SessionRevision,
}

impl ArchiveSession {
    /// Creates a request that rejects active leases and revision drift.
    #[must_use]
    pub const fn new(session_id: SessionId, expected_revision: SessionRevision) -> Self {
        Self {
            session_id,
            expected_revision,
        }
    }

    /// Returns the target session.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the exact expected topology revision.
    #[must_use]
    pub const fn expected_revision(self) -> SessionRevision {
        self.expected_revision
    }
}

/// Stable receipt preserving exact pre/post archive state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveReceipt {
    request: ArchiveSession,
    previous: SessionHead,
    session: SessionHead,
}

/// Exact request to irreversibly remove one archived session authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeleteSession {
    session_id: SessionId,
    expected_revision: SessionRevision,
}

impl DeleteSession {
    /// Binds deletion to one exact archived topology revision.
    #[must_use]
    pub const fn new(session_id: SessionId, expected_revision: SessionRevision) -> Self {
        Self {
            session_id,
            expected_revision,
        }
    }

    /// Returns the exact target identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the exact archived revision.
    #[must_use]
    pub const fn expected_revision(self) -> SessionRevision {
        self.expected_revision
    }
}

/// Stable acknowledgement of one accepted semantic deletion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeleteReceipt {
    request: DeleteSession,
    archived: SessionHead,
    reclaimed_checkpoints: u64,
}

impl DeleteReceipt {
    /// Creates a backend-returned exact deletion receipt.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        request: DeleteSession,
        archived: SessionHead,
        reclaimed_checkpoints: u64,
    ) -> Self {
        Self {
            request,
            archived,
            reclaimed_checkpoints,
        }
    }

    /// Returns the exact accepted request.
    #[must_use]
    pub const fn request(self) -> DeleteSession {
        self.request
    }
    /// Returns the exact archived authority that was removed.
    #[must_use]
    pub const fn archived(self) -> SessionHead {
        self.archived
    }
    /// Returns removed immutable checkpoint roots.
    #[must_use]
    pub const fn reclaimed_checkpoints(self) -> u64 {
        self.reclaimed_checkpoints
    }
}

/// Exact request to reclaim superseded checkpoint history of one branch.
///
/// Only the live owner may prune, only at its exact Quiescent head. The store
/// keeps the head, its parent, every create and fork checkpoint, every
/// checkpoint another branch depends on, and every checkpoint in `retain`;
/// everything else below the head's parent is removed with its commit receipt
/// and queues objects no retained checkpoint reaches for deferred reclamation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PruneBranch {
    lease: BranchLease,
    expected: BranchHead,
    retain: BTreeSet<CheckpointId>,
    minimum: u64,
}

impl PruneBranch {
    /// Binds one prune to the owner's lease and exact head. Nothing is removed
    /// unless at least `minimum` checkpoints would be.
    #[must_use]
    pub const fn new(
        lease: BranchLease,
        expected: BranchHead,
        retain: BTreeSet<CheckpointId>,
        minimum: u64,
    ) -> Self {
        Self {
            lease,
            expected,
            retain,
            minimum,
        }
    }

    /// Returns the exact live owner lease.
    #[must_use]
    pub const fn lease(&self) -> &BranchLease {
        &self.lease
    }

    /// Returns the exact expected Quiescent head.
    #[must_use]
    pub const fn expected(&self) -> BranchHead {
        self.expected
    }

    /// Returns checkpoints the runtime still names and must keep.
    #[must_use]
    pub const fn retain(&self) -> &BTreeSet<CheckpointId> {
        &self.retain
    }

    /// Returns the least number of checkpoints worth one prune.
    #[must_use]
    pub const fn minimum(&self) -> u64 {
        self.minimum
    }
}

/// Checkpoint removals acknowledged by one prune. Object reclaim is separate.
/// Repeating a prune at the same head removes
/// nothing more and reports zeros.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PruneReceipt {
    head: BranchHead,
    horizon: BranchRevision,
    pruned_checkpoints: u64,
}

impl PruneReceipt {
    /// Creates a backend-returned prune receipt.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        head: BranchHead,
        horizon: BranchRevision,
        pruned_checkpoints: u64,
    ) -> Self {
        Self {
            head,
            horizon,
            pruned_checkpoints,
        }
    }

    /// Returns the unchanged head the prune ran at.
    #[must_use]
    pub const fn head(self) -> BranchHead {
        self.head
    }

    /// Returns the branch history horizon: no commit receipt is retained at
    /// or below it, and no checkpoint below it except retained ones.
    #[must_use]
    pub const fn horizon(self) -> BranchRevision {
        self.horizon
    }

    /// Returns removed checkpoints.
    #[must_use]
    pub const fn pruned_checkpoints(self) -> u64 {
        self.pruned_checkpoints
    }
}

impl ArchiveReceipt {
    /// Creates a backend-returned archive receipt.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        request: ArchiveSession,
        previous: SessionHead,
        session: SessionHead,
    ) -> Self {
        Self {
            request,
            previous,
            session,
        }
    }

    /// Returns the exact accepted request.
    #[must_use]
    pub const fn request(self) -> ArchiveSession {
        self.request
    }

    /// Returns the exact pre-archive metadata.
    #[must_use]
    pub const fn previous(self) -> SessionHead {
        self.previous
    }

    /// Returns the archived metadata.
    #[must_use]
    pub const fn session(self) -> SessionHead {
        self.session
    }
}

/// Truthful store barrier status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilityReport {
    effective: RuntimeDurability,
    unresolved_commits: u64,
    oldest_unresolved: Option<CheckpointId>,
}

impl DurabilityReport {
    /// Creates a backend-returned report.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        effective: RuntimeDurability,
        unresolved_commits: u64,
        oldest_unresolved: Option<CheckpointId>,
    ) -> Self {
        Self {
            effective,
            unresolved_commits,
            oldest_unresolved,
        }
    }

    /// Returns the effective persistence guarantee.
    #[must_use]
    pub const fn effective(self) -> RuntimeDurability {
        self.effective
    }

    /// Returns the bounded unresolved commit count.
    #[must_use]
    pub const fn unresolved_commits(self) -> u64 {
        self.unresolved_commits
    }

    /// Returns the oldest unresolved commit identity.
    #[must_use]
    pub const fn oldest_unresolved(self) -> Option<CheckpointId> {
        self.oldest_unresolved
    }

    /// Reports whether the barrier has no unresolved commit debt.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.unresolved_commits == 0 && self.oldest_unresolved.is_none()
    }
}

/// Amount of deferred reclaim requested independently of semantic deletion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreReclaimMode {
    /// One bounded object/byte batch, used before a pressure refusal.
    Bounded,
    /// Repeated bounded transactions until the persistent queue is empty.
    Drain,
}

/// Reclaim actually committed by an explicit maintenance request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreReclaimReport {
    objects: u64,
    object_bytes: u64,
    pending: bool,
}

impl StoreReclaimReport {
    /// Constructs a backend maintenance report.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(objects: u64, object_bytes: u64, pending: bool) -> Self {
        Self {
            objects,
            object_bytes,
            pending,
        }
    }
    /// Returns deleted content-addressed objects.
    #[must_use]
    pub const fn objects(self) -> u64 {
        self.objects
    }
    /// Returns deleted payload and edge bytes.
    #[must_use]
    pub const fn object_bytes(self) -> u64 {
        self.object_bytes
    }
    /// Whether another bounded transaction is needed.
    #[must_use]
    pub const fn pending(self) -> bool {
        self.pending
    }
}
