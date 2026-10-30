use super::{
    BranchHead, BranchLease, CheckpointObjectBundle, CheckpointObjectRef, Digest, FeedFact,
    FeedSeq, NonZeroU64, RunId, SessionHead, SessionRef, StoreAdmission, Timestamp,
};

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
