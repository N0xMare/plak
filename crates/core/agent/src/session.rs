//! Semantic session-store contract owned by the agent runtime.
//!
//! Backends publish immutable [`RuntimeCheckpoint`] object bundles under exact
//! branch CAS and lease fencing. They never interpret canonical history,
//! operation-ledger, family-artifact, event, transcript, or presentation state.

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
pub use plakous_model::SESSION_WORKFLOW_ROOT_ID_BYTES as SESSION_WORKFLOW_ID_BYTES;
/// The one canonical caller-stable workflow/root identity used by model calls,
/// manifests, and session-store receipts.
pub use plakous_model::SessionWorkflowRootId as SessionWorkflowId;

/// Boxed future returned by object-safe store operations.
pub type StoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SessionStoreError>> + Send + 'a>>;

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

/// An opaque, monotonically fenced branch ownership token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchLease {
    session: SessionRef,
    owner: RunId,
    fence: NonZeroU64,
    expires_at: LeaseInstant,
    lease_seconds: NonZeroU64,
}

impl BranchLease {
    /// Creates a backend-minted lease. Possession alone grants no authority;
    /// mutations authenticate exact backend state, while dispatch admission
    /// checks the store's granted interval using [`SessionStore::lease_now`].
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        session: SessionRef,
        owner: RunId,
        fence: NonZeroU64,
        expires_at: LeaseInstant,
        lease_seconds: NonZeroU64,
    ) -> Self {
        Self {
            session,
            owner,
            fence,
            expires_at,
            lease_seconds,
        }
    }

    /// Returns the owned branch.
    #[must_use]
    pub const fn session(&self) -> SessionRef {
        self.session
    }

    /// Returns the exact run owner.
    #[must_use]
    pub const fn owner(&self) -> RunId {
        self.owner
    }

    /// Returns the monotonically increasing fence.
    #[must_use]
    pub const fn fence(&self) -> NonZeroU64 {
        self.fence
    }

    /// Returns the deadline in the store's lease authority domain.
    #[must_use]
    pub const fn expires_at(&self) -> LeaseInstant {
        self.expires_at
    }

    /// Returns the renewal duration.
    #[must_use]
    pub const fn lease_seconds(&self) -> NonZeroU64 {
        self.lease_seconds
    }
}

/// Atomic session creation, genesis checkpoint, and fence-1 lease request.
#[derive(Clone, Debug)]
pub struct CreateSession {
    session: SessionRef,
    created_at: Timestamp,
    owner: RunId,
    lease_seconds: NonZeroU64,
    initial: CheckpointObjectBundle,
    admission: StoreAdmission,
    feed_facts: Vec<FeedFact>,
}

impl CreateSession {
    /// Creates an exact semantic genesis request.
    #[must_use]
    pub const fn new(
        session: SessionRef,
        created_at: Timestamp,
        owner: RunId,
        lease_seconds: NonZeroU64,
        initial: CheckpointObjectBundle,
    ) -> Self {
        Self {
            session,
            created_at,
            owner,
            lease_seconds,
            initial,
            admission: StoreAdmission::Ordinary,
            feed_facts: Vec::new(),
        }
    }

    /// Selects the retention reserve this creation may consume.
    #[must_use]
    pub fn with_admission(mut self, admission: StoreAdmission) -> Self {
        self.admission = admission;
        self
    }

    /// Supplies the facts of the genesis record the store appends.
    #[must_use]
    pub fn with_feed_facts(mut self, facts: Vec<FeedFact>) -> Self {
        self.feed_facts = facts;
        self
    }

    /// Returns the facts of the genesis record.
    #[must_use]
    pub fn feed_facts(&self) -> &[FeedFact] {
        &self.feed_facts
    }

    /// Returns the retention admission class.
    #[must_use]
    pub const fn admission(&self) -> StoreAdmission {
        self.admission
    }

    /// Returns the target session and initial branch.
    #[must_use]
    pub const fn session(&self) -> SessionRef {
        self.session
    }

    /// Returns the exact creation time.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Returns the initial lease owner.
    #[must_use]
    pub const fn owner(&self) -> RunId {
        self.owner
    }

    /// Returns the initial lease duration.
    #[must_use]
    pub const fn lease_seconds(&self) -> NonZeroU64 {
        self.lease_seconds
    }

    /// Returns the complete semantic genesis transfer bundle.
    #[must_use]
    pub const fn initial(&self) -> &CheckpointObjectBundle {
        &self.initial
    }
}

/// Stable exact identity of an accepted create operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateOperation {
    session: SessionRef,
    created_at: Timestamp,
    owner: RunId,
    lease_seconds: NonZeroU64,
    initial: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
}

impl CreateOperation {
    /// Creates a backend-validated operation binding.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        session: SessionRef,
        created_at: Timestamp,
        owner: RunId,
        lease_seconds: NonZeroU64,
        initial: CheckpointObjectRef,
        checkpoint_commit_identity: Digest,
    ) -> Self {
        Self {
            session,
            created_at,
            owner,
            lease_seconds,
            initial,
            checkpoint_commit_identity,
        }
    }

    /// Returns the target session and branch.
    #[must_use]
    pub const fn session(&self) -> SessionRef {
        self.session
    }

    /// Returns the exact creation time.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Returns the exact initial lease owner.
    #[must_use]
    pub const fn owner(&self) -> RunId {
        self.owner
    }

    /// Returns the exact initial lease duration.
    #[must_use]
    pub const fn lease_seconds(&self) -> NonZeroU64 {
        self.lease_seconds
    }

    /// Returns the exact genesis checkpoint reference.
    #[must_use]
    pub const fn initial(&self) -> CheckpointObjectRef {
        self.initial
    }

    /// Returns the manifest-bound commit identity.
    #[must_use]
    pub const fn checkpoint_commit_identity(&self) -> Digest {
        self.checkpoint_commit_identity
    }
}

/// Stable receipt for one atomically created session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateReceipt {
    operation: CreateOperation,
    session: SessionHead,
    branch: BranchHead,
    lease: BranchLease,
    feed_seq: FeedSeq,
}

impl CreateReceipt {
    /// Creates a backend-returned receipt.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        operation: CreateOperation,
        session: SessionHead,
        branch: BranchHead,
        lease: BranchLease,
        feed_seq: FeedSeq,
    ) -> Self {
        Self {
            operation,
            session,
            branch,
            lease,
            feed_seq,
        }
    }

    /// Returns the sequence of the `Created` record that made this create
    /// durable.
    #[must_use]
    pub const fn feed_seq(&self) -> FeedSeq {
        self.feed_seq
    }

    /// Returns the exact accepted operation identity.
    #[must_use]
    pub const fn operation(&self) -> &CreateOperation {
        &self.operation
    }

    /// Returns resulting session metadata.
    #[must_use]
    pub const fn session(&self) -> SessionHead {
        self.session
    }

    /// Returns the genesis branch head.
    #[must_use]
    pub const fn branch(&self) -> BranchHead {
        self.branch
    }

    /// Returns the atomically acquired fence-1 lease.
    #[must_use]
    pub const fn lease(&self) -> &BranchLease {
        &self.lease
    }
}

/// Request to acquire one branch for an exact run owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcquireBranch {
    session: SessionRef,
    owner: RunId,
    lease_seconds: NonZeroU64,
}

impl AcquireBranch {
    /// Creates an exact acquisition request.
    #[must_use]
    pub const fn new(session: SessionRef, owner: RunId, lease_seconds: NonZeroU64) -> Self {
        Self {
            session,
            owner,
            lease_seconds,
        }
    }

    /// Returns the target branch.
    #[must_use]
    pub const fn session(self) -> SessionRef {
        self.session
    }

    /// Returns the exact run owner.
    #[must_use]
    pub const fn owner(self) -> RunId {
        self.owner
    }

    /// Returns the requested duration.
    #[must_use]
    pub const fn lease_seconds(self) -> NonZeroU64 {
        self.lease_seconds
    }
}

/// Closed semantic meaning of one requested new-branch transition.
///
/// All variants require a typed runtime context-state transition before a
/// backend may publish them. The enum is nevertheless part of the exact
/// operation identity now, so a future supported transition cannot be confused
/// with an acknowledgement-lost request made under another mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BranchTransitionMode {
    /// Preserve the exact source semantics while rotating branch context.
    #[default]
    OrdinaryFork,
    /// Recompile and admit an exact new binding and plan on a new branch.
    Rebind,
}

/// Exact quiescent new-branch request with target checkpoint and fence-1 lease.
#[derive(Clone, Debug)]
pub struct ForkBranch {
    workflow_id: SessionWorkflowId,
    semantic_request: Digest,
    source_lease: BranchLease,
    expected_session_revision: SessionRevision,
    source: BranchHead,
    target_branch: BranchId,
    owner: RunId,
    lease_seconds: NonZeroU64,
    transition: BranchTransitionMode,
    target: CheckpointObjectBundle,
    admission: StoreAdmission,
    feed_facts: Vec<FeedFact>,
}

impl ForkBranch {
    /// Creates an exact atomic fork request.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed fork CAS"
    )]
    #[must_use]
    pub const fn new(
        workflow_id: SessionWorkflowId,
        semantic_request: Digest,
        source_lease: BranchLease,
        expected_session_revision: SessionRevision,
        source: BranchHead,
        target_branch: BranchId,
        owner: RunId,
        lease_seconds: NonZeroU64,
        target: CheckpointObjectBundle,
    ) -> Self {
        Self {
            workflow_id,
            semantic_request,
            source_lease,
            expected_session_revision,
            source,
            target_branch,
            owner,
            lease_seconds,
            transition: BranchTransitionMode::OrdinaryFork,
            target,
            admission: StoreAdmission::Ordinary,
            feed_facts: Vec::new(),
        }
    }

    /// Selects the retention reserve this fork may consume.
    #[must_use]
    pub fn with_admission(mut self, admission: StoreAdmission) -> Self {
        self.admission = admission;
        self
    }

    /// Supplies the facts of the `Branched` record the store appends.
    #[must_use]
    pub fn with_feed_facts(mut self, facts: Vec<FeedFact>) -> Self {
        self.feed_facts = facts;
        self
    }

    /// Returns the facts of the `Branched` record.
    #[must_use]
    pub fn feed_facts(&self) -> &[FeedFact] {
        &self.feed_facts
    }

    /// Returns the retention admission class.
    #[must_use]
    pub const fn admission(&self) -> StoreAdmission {
        self.admission
    }

    /// Returns the stable workflow identity used for retry and recovery.
    #[must_use]
    pub const fn workflow_id(&self) -> SessionWorkflowId {
        self.workflow_id
    }

    /// Returns the immutable high-level semantic request binding.
    #[must_use]
    pub const fn semantic_request(&self) -> Digest {
        self.semantic_request
    }

    /// Selects one closed semantic new-branch transition mode.
    #[must_use]
    pub fn with_transition(mut self, transition: BranchTransitionMode) -> Self {
        self.transition = transition;
        self
    }

    /// Returns the exact source lease.
    #[must_use]
    pub const fn source_lease(&self) -> &BranchLease {
        &self.source_lease
    }

    /// Returns the expected session-topology revision.
    #[must_use]
    pub const fn expected_session_revision(&self) -> SessionRevision {
        self.expected_session_revision
    }

    /// Returns the exact source branch head.
    #[must_use]
    pub const fn source(&self) -> BranchHead {
        self.source
    }

    /// Returns the target branch identity.
    #[must_use]
    pub const fn target_branch(&self) -> BranchId {
        self.target_branch
    }

    /// Returns the target lease owner.
    #[must_use]
    pub const fn owner(&self) -> RunId {
        self.owner
    }

    /// Returns the target lease duration.
    #[must_use]
    pub const fn lease_seconds(&self) -> NonZeroU64 {
        self.lease_seconds
    }

    /// Returns the closed requested transition mode.
    #[must_use]
    pub const fn transition(&self) -> BranchTransitionMode {
        self.transition
    }

    /// Returns the target checkpoint transfer bundle.
    #[must_use]
    pub const fn target(&self) -> &CheckpointObjectBundle {
        &self.target
    }
}

/// Exact stable fork-receipt recovery request under a current source fence.
///
/// Recovery deliberately omits the original session-topology CAS revision,
/// target checkpoint bundle, and opaque branch-context entropy. The backend
/// loads the accepted receipt and materialized source/target checkpoints by
/// [`SessionWorkflowId`].
#[derive(Clone, Debug)]
pub struct RecoverFork {
    workflow_id: SessionWorkflowId,
    semantic_request: Digest,
    source_lease: BranchLease,
    source: BranchHead,
    target_branch: BranchId,
    transition: BranchTransitionMode,
}

impl RecoverFork {
    /// Creates an exact ordinary-fork recovery request.
    #[must_use]
    pub const fn new(
        workflow_id: SessionWorkflowId,
        semantic_request: Digest,
        source_lease: BranchLease,
        source: BranchHead,
        target_branch: BranchId,
    ) -> Self {
        Self {
            workflow_id,
            semantic_request,
            source_lease,
            source,
            target_branch,
            transition: BranchTransitionMode::OrdinaryFork,
        }
    }

    /// Selects the exact accepted transition mode to recover.
    #[must_use]
    pub fn with_transition(mut self, transition: BranchTransitionMode) -> Self {
        self.transition = transition;
        self
    }

    /// Returns the stable workflow identity.
    #[must_use]
    pub const fn workflow_id(&self) -> SessionWorkflowId {
        self.workflow_id
    }

    /// Returns the immutable high-level semantic request binding.
    #[must_use]
    pub const fn semantic_request(&self) -> Digest {
        self.semantic_request
    }

    /// Returns the current exact source lease used only for recovery authority.
    #[must_use]
    pub const fn source_lease(&self) -> &BranchLease {
        &self.source_lease
    }

    /// Returns the exact accepted source head.
    #[must_use]
    pub const fn source(&self) -> BranchHead {
        self.source
    }

    /// Returns the exact accepted target branch identity.
    #[must_use]
    pub const fn target_branch(&self) -> BranchId {
        self.target_branch
    }

    /// Returns the exact accepted transition mode.
    #[must_use]
    pub const fn transition(&self) -> BranchTransitionMode {
        self.transition
    }
}

/// Stable exact identity of one accepted fork.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkOperation {
    workflow_id: SessionWorkflowId,
    semantic_request: Digest,
    expected_session_revision: SessionRevision,
    source: BranchHead,
    target: SessionRef,
    transition: BranchTransitionMode,
    checkpoint: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
}

impl ForkOperation {
    /// Creates a backend-validated fork binding.
    #[doc(hidden)]
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed fork identity"
    )]
    #[must_use]
    pub const fn from_store(
        workflow_id: SessionWorkflowId,
        semantic_request: Digest,
        expected_session_revision: SessionRevision,
        source: BranchHead,
        target: SessionRef,
        transition: BranchTransitionMode,
        checkpoint: CheckpointObjectRef,
        checkpoint_commit_identity: Digest,
    ) -> Self {
        Self {
            workflow_id,
            semantic_request,
            expected_session_revision,
            source,
            target,
            transition,
            checkpoint,
            checkpoint_commit_identity,
        }
    }

    /// Returns the stable workflow identity.
    #[must_use]
    pub const fn workflow_id(&self) -> SessionWorkflowId {
        self.workflow_id
    }

    /// Returns the immutable high-level semantic request binding.
    #[must_use]
    pub const fn semantic_request(&self) -> Digest {
        self.semantic_request
    }

    /// Returns the exact source head.
    #[must_use]
    pub const fn source(&self) -> BranchHead {
        self.source
    }

    /// Returns the historical source-session topology revision used at publication.
    #[must_use]
    pub const fn expected_session_revision(&self) -> SessionRevision {
        self.expected_session_revision
    }

    /// Returns the exact target branch.
    #[must_use]
    pub const fn target(&self) -> SessionRef {
        self.target
    }

    /// Returns the exact accepted new-branch transition mode.
    #[must_use]
    pub const fn transition(&self) -> BranchTransitionMode {
        self.transition
    }

    /// Returns the target checkpoint reference.
    #[must_use]
    pub const fn checkpoint(&self) -> CheckpointObjectRef {
        self.checkpoint
    }

    /// Returns the target checkpoint's manifest-bound commit identity.
    #[must_use]
    pub const fn checkpoint_commit_identity(&self) -> Digest {
        self.checkpoint_commit_identity
    }
}

/// Stable receipt for one atomic quiescent fork.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkReceipt {
    operation: ForkOperation,
    previous_session: SessionHead,
    session: SessionHead,
    source: BranchHead,
    target: BranchHead,
    lease: BranchLease,
    feed_seq: FeedSeq,
}

impl ForkReceipt {
    /// Creates a backend-returned fork receipt.
    #[doc(hidden)]
    #[allow(
        clippy::too_many_arguments,
        reason = "the receipt preserves exact old/new authority"
    )]
    #[must_use]
    pub const fn from_store(
        operation: ForkOperation,
        previous_session: SessionHead,
        session: SessionHead,
        source: BranchHead,
        target: BranchHead,
        lease: BranchLease,
        feed_seq: FeedSeq,
    ) -> Self {
        Self {
            operation,
            previous_session,
            session,
            source,
            target,
            lease,
            feed_seq,
        }
    }

    /// Returns the sequence of the `Branched` record that made this fork
    /// durable.
    #[must_use]
    pub const fn feed_seq(&self) -> FeedSeq {
        self.feed_seq
    }

    /// Returns the exact accepted operation.
    #[must_use]
    pub const fn operation(&self) -> &ForkOperation {
        &self.operation
    }

    /// Returns session metadata before publication.
    #[must_use]
    pub const fn previous_session(&self) -> SessionHead {
        self.previous_session
    }

    /// Returns resulting session metadata.
    #[must_use]
    pub const fn session(&self) -> SessionHead {
        self.session
    }

    /// Returns the unchanged exact source head.
    #[must_use]
    pub const fn source(&self) -> BranchHead {
        self.source
    }

    /// Returns the new independent target head.
    #[must_use]
    pub const fn target(&self) -> BranchHead {
        self.target
    }

    /// Returns the exact target lease acquired by the original publication.
    ///
    /// A recovered receipt may carry an expired, released, or superseded
    /// lease. It is cleanup evidence, not current mutation authority; callers
    /// acquire a fresh target lease before using the branch.
    #[must_use]
    pub const fn lease(&self) -> &BranchLease {
        &self.lease
    }
}

/// Exact fenced branch-head CAS request for one immutable checkpoint.
#[derive(Clone, Debug)]
pub struct CommitCheckpoint {
    lease: BranchLease,
    expected: BranchHead,
    checkpoint: CheckpointObjectBundle,
    admission: StoreAdmission,
    feed_facts: Vec<FeedFact>,
}

impl CommitCheckpoint {
    /// Creates an exact checkpoint publication request.
    #[must_use]
    pub const fn new(
        lease: BranchLease,
        expected: BranchHead,
        checkpoint: CheckpointObjectBundle,
    ) -> Self {
        Self {
            lease,
            expected,
            checkpoint,
            admission: StoreAdmission::Ordinary,
            feed_facts: Vec::new(),
        }
    }

    /// Selects the retention reserve this commit may consume.
    #[must_use]
    pub fn with_admission(mut self, admission: StoreAdmission) -> Self {
        self.admission = admission;
        self
    }

    /// Supplies the facts of the `Advanced` record the store appends.
    #[must_use]
    pub fn with_feed_facts(mut self, facts: Vec<FeedFact>) -> Self {
        self.feed_facts = facts;
        self
    }

    /// Returns the facts of the `Advanced` record.
    #[must_use]
    pub fn feed_facts(&self) -> &[FeedFact] {
        &self.feed_facts
    }

    /// Returns the retention admission class.
    #[must_use]
    pub const fn admission(&self) -> StoreAdmission {
        self.admission
    }

    /// Returns the exact lease and fence.
    #[must_use]
    pub const fn lease(&self) -> &BranchLease {
        &self.lease
    }

    /// Returns the exact old branch head CAS value.
    #[must_use]
    pub const fn expected(&self) -> BranchHead {
        self.expected
    }

    /// Returns the semantic checkpoint transfer bundle.
    #[must_use]
    pub const fn checkpoint(&self) -> &CheckpointObjectBundle {
        &self.checkpoint
    }
}

/// Stable exact identity of an accepted checkpoint commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOperation {
    lease: BranchLease,
    expected: BranchHead,
    checkpoint: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
}

impl CommitOperation {
    /// Creates a backend-validated commit binding.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        lease: BranchLease,
        expected: BranchHead,
        checkpoint: CheckpointObjectRef,
        checkpoint_commit_identity: Digest,
    ) -> Self {
        Self {
            lease,
            expected,
            checkpoint,
            checkpoint_commit_identity,
        }
    }

    /// Returns the exact lease and fence.
    #[must_use]
    pub const fn lease(&self) -> &BranchLease {
        &self.lease
    }

    /// Returns the exact old head CAS value.
    #[must_use]
    pub const fn expected(&self) -> BranchHead {
        self.expected
    }

    /// Returns the exact committed object root.
    #[must_use]
    pub const fn checkpoint(&self) -> CheckpointObjectRef {
        self.checkpoint
    }

    /// Returns the checkpoint's manifest-bound commit identity.
    #[must_use]
    pub const fn checkpoint_commit_identity(&self) -> Digest {
        self.checkpoint_commit_identity
    }
}

/// Stable receipt preserving the exact old and new branch heads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    checkpoint: CheckpointObjectRef,
    previous: BranchHead,
    head: BranchHead,
    feed_seq: FeedSeq,
}

impl CommitReceipt {
    /// Creates a backend-returned commit receipt.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        checkpoint: CheckpointObjectRef,
        previous: BranchHead,
        head: BranchHead,
        feed_seq: FeedSeq,
    ) -> Self {
        Self {
            checkpoint,
            previous,
            head,
            feed_seq,
        }
    }

    /// Returns the sequence of the `Advanced` record that made this commit
    /// durable.
    #[must_use]
    pub const fn feed_seq(self) -> FeedSeq {
        self.feed_seq
    }

    /// Returns the exact committed checkpoint.
    #[must_use]
    pub const fn checkpoint(self) -> CheckpointObjectRef {
        self.checkpoint
    }

    /// Returns the exact pre-commit head.
    #[must_use]
    pub const fn previous(self) -> BranchHead {
        self.previous
    }

    /// Returns the exact resulting head.
    #[must_use]
    pub const fn head(self) -> BranchHead {
        self.head
    }
}

/// Exact accepted commit operation and its acknowledgement-stable receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedCheckpoint {
    operation: CommitOperation,
    receipt: CommitReceipt,
}

impl CommittedCheckpoint {
    /// Creates a backend-returned recovery record.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(operation: CommitOperation, receipt: CommitReceipt) -> Self {
        Self { operation, receipt }
    }

    /// Returns the exact accepted semantic request binding.
    #[must_use]
    pub const fn operation(&self) -> &CommitOperation {
        &self.operation
    }

    /// Returns the stable accepted receipt.
    #[must_use]
    pub const fn receipt(&self) -> CommitReceipt {
        self.receipt
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

/// Closed semantic reason for refusing a session-store request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum StoreInvalidReason {
    /// Branch revision exhausted.
    #[serde(rename = "branch revision exhausted")]
    #[error("branch revision exhausted")]
    BranchRevisionExhausted,
    /// Checkpoint commit topology.
    #[serde(rename = "checkpoint commit topology")]
    #[error("checkpoint commit topology")]
    CheckpointCommitTopology,
    /// Checkpoint object binding.
    #[serde(rename = "checkpoint object binding")]
    #[error("checkpoint object binding")]
    CheckpointObjectBinding,
    /// Checkpoint object bundle.
    #[serde(rename = "checkpoint object bundle")]
    #[error("checkpoint object bundle")]
    CheckpointObjectBundle,
    /// Checkpoint objects are not canonical.
    #[serde(rename = "checkpoint objects are not canonical")]
    #[error("checkpoint objects are not canonical")]
    NoncanonicalCheckpointObjects,
    /// Disabled durability requires an in-memory session store.
    #[serde(rename = "disabled durability requires an in-memory session store")]
    #[error("disabled durability requires an in-memory session store")]
    DisabledDurabilityRequiresMemoryStore,
    /// Fork checkpoint semantics.
    #[serde(rename = "fork checkpoint semantics")]
    #[error("fork checkpoint semantics")]
    ForkCheckpointSemantics,
    /// Fork semantic request digest domain.
    #[serde(rename = "fork semantic request digest domain")]
    #[error("fork semantic request digest domain")]
    ForkRequestDigestDomain,
    /// Fork source is not quiescent.
    #[serde(rename = "fork source is not quiescent")]
    #[error("fork source is not quiescent")]
    ForkSourceNotQuiescent,
    /// Genesis checkpoint topology.
    #[serde(rename = "genesis checkpoint topology")]
    #[error("genesis checkpoint topology")]
    GenesisCheckpointTopology,
    /// Invalid typed branch context transition.
    #[serde(rename = "invalid typed branch context transition")]
    #[error("invalid typed branch context transition")]
    InvalidBranchContextTransition,
    /// Lease expiry.
    #[serde(rename = "lease expiry")]
    #[error("lease expiry")]
    LeaseExpiry,
    /// Lease expiry overflow.
    #[serde(rename = "lease expiry overflow")]
    #[error("lease expiry overflow")]
    LeaseExpiryOverflow,
    /// Manifest root delta.
    #[serde(rename = "manifest root delta")]
    #[error("manifest root delta")]
    ManifestRootDelta,
    /// Ordinary fork changed source semantics.
    #[serde(rename = "ordinary fork changed source semantics")]
    #[error("ordinary fork changed source semantics")]
    OrdinaryForkChangedSemantics,
    /// Rebind did not change a bound configuration fact.
    #[serde(rename = "rebind did not change a bound configuration fact")]
    #[error("rebind did not change a bound configuration fact")]
    RebindConfigurationUnchanged,
    /// Prune head is not quiescent.
    #[serde(rename = "prune head is not quiescent")]
    #[error("prune head is not quiescent")]
    PruneHeadNotQuiescent,
    /// Session is not archived.
    #[serde(rename = "session is not archived")]
    #[error("session is not archived")]
    SessionNotArchived,
    /// Unauthorized branch manifest transition.
    #[serde(rename = "unauthorized branch manifest transition")]
    #[error("unauthorized branch manifest transition")]
    UnauthorizedBranchManifestTransition,
}

/// Closed resource or numeric bound enforced by a session store.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum StoreCapacityBound {
    /// Branch revision.
    #[serde(rename = "branch revision")]
    #[error("branch revision")]
    BranchRevision,
    /// Session revision.
    #[serde(rename = "session revision")]
    #[error("session revision")]
    SessionRevision,
    /// Checkpoints.
    #[serde(rename = "checkpoints")]
    #[error("checkpoints")]
    Checkpoints,
    /// Lease fence.
    #[serde(rename = "lease fence")]
    #[error("lease fence")]
    LeaseFence,
    /// Object bytes.
    #[serde(rename = "object bytes")]
    #[error("object bytes")]
    ObjectBytes,
    /// Objects.
    #[serde(rename = "objects")]
    #[error("objects")]
    Objects,
    /// Page size.
    #[serde(rename = "page size")]
    #[error("page size")]
    PageSize,
    /// Sessions.
    #[serde(rename = "sessions")]
    #[error("sessions")]
    Sessions,
    /// Branches per session.
    #[serde(rename = "branches per session")]
    #[error("branches per session")]
    BranchesPerSession,
    /// `SQLite` integer.
    #[serde(rename = "SQLite integer")]
    #[error("SQLite integer")]
    SqliteInteger,
    /// Encoded bytes of one change-feed record.
    #[serde(rename = "feed record bytes")]
    #[error("feed record bytes")]
    FeedRecordBytes,
}

/// Which share of an aggregate retention bound one store write may consume.
///
/// Every aggregate retention bound (checkpoints, objects, object bytes) keeps
/// a reserve of `ceil(limit / 8)`. Work that begins something new stops before
/// the reserve; recording the outcome of work already dispatched may use half
/// of it; recovery may use all of it. The caller that owns the write's meaning
/// chooses the class; it is never a user setting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StoreAdmission {
    /// Begins new work: a root turn, a prompt, a prepare record, the first
    /// fence of an operation out of `Prepared` (later transitions of an
    /// operation that crossed it are settlement), a session creation or fork.
    #[default]
    Ordinary,
    /// Records the outcome of already-dispatched work or publishes a turn's
    /// terminal result.
    Settlement,
    /// Open-time abandonment, explicit recovery and recovery evidence.
    Recovery,
}

impl StoreAdmission {
    /// Returns the reserve kept behind an aggregate `limit`.
    #[must_use]
    pub const fn reserve(limit: u64) -> u64 {
        limit.div_ceil(8)
    }

    /// Returns how much of an aggregate `limit` this class may fill.
    #[must_use]
    pub const fn threshold(self, limit: u64) -> u64 {
        let reserve = Self::reserve(limit);
        match self {
            Self::Ordinary => limit - reserve,
            Self::Settlement => limit - reserve / 2,
            Self::Recovery => limit,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Settlement => "settlement",
            Self::Recovery => "recovery",
        }
    }
}

impl fmt::Display for StoreAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Which bounded store resource was exhausted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStoreCapacity {
    /// Stable redaction-safe bound name.
    pub bound: StoreCapacityBound,
    /// Current or attempted count.
    pub current: u64,
    /// The maximum this request was held to, or zero for numeric exhaustion.
    /// For an admission-classed aggregate bound it is the class threshold.
    pub max: u64,
    /// Configured bound; equal to `max` unless an admission class applied.
    pub limit: u64,
    /// The admission class held to `max`, when an aggregate reserve applied.
    pub admission: Option<StoreAdmission>,
}

impl fmt::Display for SessionStoreCapacity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.max == 0 {
            return write!(formatter, "{} overflow", self.bound);
        }
        match self.admission {
            Some(admission) if self.max != self.limit => write!(
                formatter,
                "{} {}/{} ({admission} writes stop at {})",
                self.bound, self.current, self.limit, self.max
            ),
            _ => write!(formatter, "{} {}/{}", self.bound, self.current, self.max),
        }
    }
}

/// Used and configured amount of one aggregate retention bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreBoundUsage {
    bound: StoreCapacityBound,
    used: u64,
    limit: u64,
}

impl StoreBoundUsage {
    /// Creates one backend-reported bound reading.
    #[must_use]
    pub const fn new(bound: StoreCapacityBound, used: u64, limit: u64) -> Self {
        Self { bound, used, limit }
    }

    /// Returns which bound this reading is for.
    #[must_use]
    pub const fn bound(self) -> StoreCapacityBound {
        self.bound
    }

    /// Returns the retained amount.
    #[must_use]
    pub const fn used(self) -> u64 {
        self.used
    }

    /// Returns the configured bound.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }

    /// Returns the reserve ordinary work may not consume.
    #[must_use]
    pub const fn reserve(self) -> u64 {
        StoreAdmission::reserve(self.limit)
    }

    /// Returns the amount at which ordinary work stops.
    #[must_use]
    pub const fn ordinary_threshold(self) -> u64 {
        StoreAdmission::Ordinary.threshold(self.limit)
    }

    /// Returns the amount at which settlement stops.
    #[must_use]
    pub const fn settlement_threshold(self) -> u64 {
        StoreAdmission::Settlement.threshold(self.limit)
    }

    /// Whether ordinary work has no headroom left on this bound.
    #[must_use]
    pub const fn ordinary_exhausted(self) -> bool {
        self.used >= self.ordinary_threshold()
    }

    /// Returns the capacity refusal ordinary work meets on this bound.
    #[must_use]
    pub const fn ordinary_refusal(self) -> SessionStoreCapacity {
        SessionStoreCapacity {
            bound: self.bound,
            current: self.used,
            max: self.ordinary_threshold(),
            limit: self.limit,
            admission: Some(StoreAdmission::Ordinary),
        }
    }
}

/// Aggregate retention usage of one whole store, across every session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreUsage {
    checkpoints: StoreBoundUsage,
    objects: StoreBoundUsage,
    object_bytes: StoreBoundUsage,
}

impl StoreUsage {
    /// Creates a backend-reported usage reading.
    #[must_use]
    pub const fn new(
        checkpoints: (u64, u64),
        objects: (u64, u64),
        object_bytes: (u64, u64),
    ) -> Self {
        Self {
            checkpoints: StoreBoundUsage::new(
                StoreCapacityBound::Checkpoints,
                checkpoints.0,
                checkpoints.1,
            ),
            objects: StoreBoundUsage::new(StoreCapacityBound::Objects, objects.0, objects.1),
            object_bytes: StoreBoundUsage::new(
                StoreCapacityBound::ObjectBytes,
                object_bytes.0,
                object_bytes.1,
            ),
        }
    }

    /// Returns every reserved aggregate bound in a stable order.
    #[must_use]
    pub const fn bounds(self) -> [StoreBoundUsage; 3] {
        [self.checkpoints, self.objects, self.object_bytes]
    }

    /// Returns the first bound on which ordinary work has no headroom left.
    #[must_use]
    pub fn ordinary_exhausted(self) -> Option<StoreBoundUsage> {
        self.bounds()
            .into_iter()
            .find(|bound| bound.ordinary_exhausted())
    }
}

/// Content-free classification of a store integrity failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreIntegrityReason {
    /// An incremental publication's base is absent.
    #[error("missing_base")]
    MissingBase,
    /// A checkpoint's root is absent.
    #[error("missing_root")]
    MissingRoot,
    /// An object references an absent child.
    #[error("missing_child")]
    MissingChild,
    /// Other stored semantics or backend integrity are inconsistent.
    #[error("inconsistent")]
    Inconsistent,
}

/// Typed redaction-safe semantic store failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SessionStoreError {
    /// Caller input violates a closed semantic invariant.
    #[error("invalid session-store request: {0}")]
    Invalid(StoreInvalidReason),
    /// The selected session, branch, checkpoint, or receipt is absent.
    #[error("selected session-store record does not exist")]
    NotFound,
    /// An operation identity is occupied by different exact semantic bytes.
    #[error("session-store operation identity conflicts with accepted bytes")]
    IdentityConflict,
    /// Expected session/branch revision or head disagrees.
    #[error("session-store optimistic concurrency conflict")]
    Conflict,
    /// Another unexpired owner holds the branch.
    #[error("session branch is leased by another owner")]
    LeaseHeld,
    /// The supplied lease has expired.
    #[error("session branch lease expired")]
    LeaseExpired,
    /// The supplied reading or grant belongs to a different clock domain.
    #[error("session branch lease clock domain differs")]
    LeaseDomainMismatch,
    /// A newer or renewed fence value superseded the supplied token.
    #[error("session branch fence is superseded")]
    FenceSuperseded,
    /// The session is archived and closed to ordinary mutation/load.
    #[error("session is archived")]
    Archived,
    /// A configured hard bound was reached.
    #[error("session store capacity was reached: {0}")]
    Capacity(SessionStoreCapacity),
    /// Durable backend I/O or authoritative in-process state is unavailable.
    #[error("session store backend is unavailable")]
    BackendUnavailable,
    /// A bounded backend queue or lock wait is exhausted.
    #[error("session store backend is busy")]
    BackendBusy,
    /// Physical storage is exhausted, independently of logical retention limits.
    #[error("session store disk is full")]
    StorageFull,
    /// The selected checkpoint or receipt was reclaimed by history retention.
    #[error("selected checkpoint history was reclaimed by retention")]
    Pruned,
    /// Stored object bytes, graph, receipt, or topology are inconsistent.
    #[error("session store integrity check failed: {0}")]
    Corrupt(StoreIntegrityReason),
    /// Persistent data uses an unsupported schema.
    #[error("session store schema is unsupported")]
    UnsupportedSchema,
    /// Persistent semantic/object bytes use an unsupported codec.
    #[error("session store codec is unsupported")]
    UnsupportedCodec,
    /// The store has entered its terminal shutdown state.
    #[error("session store is shut down")]
    Shutdown,
}

impl SessionStoreError {
    /// Creates an integrity failure without a more specific graph reason.
    #[must_use]
    pub const fn corrupt() -> Self {
        Self::Corrupt(StoreIntegrityReason::Inconsistent)
    }

    /// Creates a structured capacity error.
    #[must_use]
    pub const fn capacity(bound: StoreCapacityBound, current: u64, max: u64) -> Self {
        Self::Capacity(SessionStoreCapacity {
            bound,
            current,
            max,
            limit: max,
            admission: None,
        })
    }

    /// Creates a capacity error for an aggregate bound held to `admission`'s
    /// share of the configured `limit`.
    #[must_use]
    pub const fn admission_capacity(
        bound: StoreCapacityBound,
        current: u64,
        limit: u64,
        admission: StoreAdmission,
    ) -> Self {
        Self::Capacity(SessionStoreCapacity {
            bound,
            current,
            max: admission.threshold(limit),
            limit,
            admission: Some(admission),
        })
    }

    /// Returns the capacity refusal this error carries, if any.
    #[must_use]
    pub const fn capacity_refusal(&self) -> Option<SessionStoreCapacity> {
        match self {
            Self::Capacity(capacity) => Some(*capacity),
            _ => None,
        }
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

/// Semantic checkpoint store used by the one private runtime owner.
///
/// Dropped futures leave a mutation wholly absent or atomically accepted under
/// its exact operation identity. Recovery never infers success from a mutable
/// branch head. After shutdown, only exact recovery reads and repeated shutdown
/// are available.
///
/// Every accepted create, commit, fork, release, prune and archive appends
/// exactly one change-feed record inside its atomic publication; renewal,
/// revival, acquisition, refused mutations and `recover_*` append nothing,
/// and an exact recovery returns the sequence the record already holds.
pub trait SessionStore: Send + Sync + 'static {
    /// Reads the lease authority clock synchronously, without I/O or actor waits.
    ///
    /// This is the exact clock used by acquire, renew and fenced mutations, not
    /// the runtime's observation clock. All clients of one backing store must
    /// share one authority domain and a coherent non-regressing timeline.
    /// Readings from another domain cannot validate a retained grant. A granted
    /// lease cannot be replaced by another owner before its deadline in that
    /// domain. The sole runtime owner serializes its own renewal and release
    /// with dispatch admission, except process-exit abandonment, which releases
    /// the lease without joining dispatched work and fences its later commits.
    ///
    /// Failure refuses new starts; historical receipts remain recoverable.
    ///
    /// # Errors
    ///
    /// Returns a backend error when its authority clock cannot be read.
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError>;

    /// Atomically creates a session, genesis checkpoint, and fence-1 lease.
    fn create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt>;

    /// Recovers one exact accepted create operation after acknowledgement loss.
    fn recover_create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt>;

    /// Loads the exact current branch head and materialized checkpoint.
    fn load_branch(&self, session: SessionRef) -> StoreFuture<'_, LoadedBranch>;

    /// Loads one exact historical immutable checkpoint.
    fn load_checkpoint(&self, checkpoint: CheckpointId) -> StoreFuture<'_, StoredCheckpoint>;

    /// Acquires or exactly reacquires one monotonically fenced branch lease.
    fn acquire(&self, request: AcquireBranch) -> StoreFuture<'_, BranchLease>;

    /// Renews only the exact current lease; retrying its pre-renewal value may
    /// return the same already-renewed value.
    fn renew(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease>;

    /// Revives this exact lease after it expired, under the same fence.
    ///
    /// Only for a holder that was suspended, not replaced: the branch must
    /// still name exactly this lease as active, its fence must still be the
    /// last one granted (so nobody acquired since), the store must still hold
    /// whatever host-local claim it took with the lease, and the clock domain
    /// must be the one the lease was granted in (a reboot is not a suspend).
    /// A lease that has not expired is renewed. A store that cannot prove all
    /// of that refuses, and the default does, with `FenceSuperseded`.
    fn revive(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        let _ = lease;
        Box::pin(async { Err(SessionStoreError::FenceSuperseded) })
    }

    /// Releases only the exact current lease; exact repetition is idempotent.
    /// Expiry limits execution authority, not relinquishment: an expired grant
    /// can be released only while its domain and exact active value still match.
    fn release(&self, lease: BranchLease) -> StoreFuture<'_, ()>;

    /// Releases a set of exact leases. Durable backends coalesce this into one transaction.
    fn release_many(&self, leases: Vec<BranchLease>) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            for lease in leases {
                self.release(lease).await?;
            }
            Ok(())
        })
    }

    /// CAS-publishes one exact immutable checkpoint under the current lease.
    /// `Corrupt`, `StorageFull`, `Invalid`, and `Capacity` must only be returned
    /// when nothing was published. If publication cannot be ruled out, return
    /// `BackendUnavailable` so the caller reconciles the exact acknowledgement.
    fn commit(&self, request: CommitCheckpoint) -> StoreFuture<'_, CommitReceipt>;

    /// Loads the exact accepted commit operation and stable receipt.
    fn recover_commit(&self, checkpoint: CheckpointId) -> StoreFuture<'_, CommittedCheckpoint>;

    /// Atomically creates one independently fenced branch from an exact
    /// quiescent source checkpoint.
    fn fork(&self, request: ForkBranch) -> StoreFuture<'_, ForkReceipt>;

    /// Recovers one exact accepted fork operation after acknowledgement loss.
    fn recover_fork(&self, request: RecoverFork) -> StoreFuture<'_, ForkReceipt>;

    /// Returns one stable bounded page of session metadata.
    fn list(&self, query: SessionQuery) -> StoreFuture<'_, Page<SessionSummary>>;

    /// Reads aggregate retention usage without changing authority. Available
    /// whenever reads are, including over a store already past its limits.
    fn usage(&self) -> StoreFuture<'_, StoreUsage>;

    /// Reclaims deferred objects without changing session or checkpoint authority.
    /// Eager backends have no deferred work and return an empty report.
    fn reclaim(&self, mode: StoreReclaimMode) -> StoreFuture<'_, StoreReclaimReport> {
        let _ = mode;
        Box::pin(async { Ok(StoreReclaimReport::default()) })
    }

    /// Archives one exact session revision after all live leases are absent.
    /// Never refused for retention capacity.
    fn archive(&self, request: ArchiveSession) -> StoreFuture<'_, ArchiveReceipt>;

    /// Deletes only one exact archived revision and retains its tombstone.
    /// Never refused for retention capacity: deletion only reclaims.
    fn delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt>;

    /// Reclaims superseded checkpoint history of one branch at its exact
    /// Quiescent head under the live owner lease, never refused for retention
    /// capacity. Loading or recovering a removed checkpoint afterwards returns
    /// [`SessionStoreError::Pruned`]. The default retains all history and
    /// reports nothing reclaimed.
    fn prune(&self, request: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
        Box::pin(async move {
            Ok(PruneReceipt::from_store(
                request.expected(),
                BranchRevision::INITIAL,
                0,
            ))
        })
    }

    /// Recovers one exact accepted deletion, including after shutdown.
    fn recover_delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt>;

    /// Reads at most `max_records` records of one session's feed after the
    /// `after` cursor, oldest first and at most [`crate::feed::MAX_FEED_PAGE_BYTES`]
    /// encoded, with the retained window observed in the same read. Needs no
    /// lease and writes nothing; available after archive and shutdown. A
    /// cursor below `retained_from - 1` reads
    /// [`FeedPage::BehindRetention`].
    fn read_feed(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> StoreFuture<'_, FeedPage>;

    /// Reads one session's retained feed window without a lease or a write.
    fn feed_tail(&self, session_id: SessionId) -> StoreFuture<'_, FeedTail>;

    /// Waits behind all accepted work and truthfully reports durability debt.
    fn flush(&self) -> StoreFuture<'_, DurabilityReport>;

    /// Stops mutation and completes the terminal durability barrier.
    ///
    /// Exact recovery reads and repeated shutdown remain available through the
    /// selected backend owner; final handle drop joins its remaining resources.
    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport>;
}
