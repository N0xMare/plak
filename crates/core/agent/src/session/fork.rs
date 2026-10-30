use super::SessionWorkflowId;
use super::{
    BranchHead, BranchId, BranchLease, CheckpointObjectBundle, CheckpointObjectRef, Digest,
    FeedFact, FeedSeq, NonZeroU64, RunId, SessionHead, SessionRef, SessionRevision, StoreAdmission,
};

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
