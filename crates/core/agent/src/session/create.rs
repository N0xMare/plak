use super::{
    BranchHead, BranchLease, CheckpointObjectBundle, CheckpointObjectRef, Digest, FeedFact,
    FeedSeq, LineOrigin, NonZeroU64, RunId, SessionHead, SessionRef, StoreAdmission, Timestamp,
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
    line_origin: LineOrigin,
    continuation: Option<LineContinuation>,
}

/// The current head a line-continuing genesis (a same-store carry) takes
/// over, fenced like a rebind: the store refuses the create unless `lease`
/// is the head's current lease, `source` is still its exact head, that head
/// is quiescent, and `consumed_terminals` are exactly the line's pending job
/// terminals, which it removes in the same transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineContinuation {
    lease: BranchLease,
    source: BranchHead,
    consumed_terminals: Vec<u32>,
}

impl LineContinuation {
    /// Names the held head, its exact checkpoint, and the folded terminals.
    #[must_use]
    pub const fn new(lease: BranchLease, source: BranchHead, consumed_terminals: Vec<u32>) -> Self {
        Self {
            lease,
            source,
            consumed_terminals,
        }
    }

    /// The current head's lease.
    #[must_use]
    pub const fn lease(&self) -> &BranchLease {
        &self.lease
    }

    /// The exact head the ledger was read from.
    #[must_use]
    pub const fn source(&self) -> BranchHead {
        self.source
    }

    /// The pending line job terminals the genesis folded.
    #[must_use]
    pub fn consumed_terminals(&self) -> &[u32] {
        &self.consumed_terminals
    }
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
            line_origin: LineOrigin::Created,
            continuation: None,
        }
    }

    /// Makes the genesis the next head of its topology's line, taking over
    /// the current head `continuation` names. A genesis whose topology
    /// continues a line is refused without one, and a new line with one.
    /// Not part of the create's exact identity: an exact replay is answered
    /// from its receipt.
    #[must_use]
    pub fn continuing_line(mut self, continuation: LineContinuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    /// Returns the head a line-continuing genesis takes over.
    #[must_use]
    pub const fn continuation(&self) -> Option<&LineContinuation> {
        self.continuation.as_ref()
    }

    /// Records where a new line came from; only [`LineOrigin::Imported`] is
    /// meaningful here. A genesis that continues a line ignores it. Provenance is not part of the create's
    /// exact identity.
    #[must_use]
    pub fn with_line_origin(mut self, origin: LineOrigin) -> Self {
        self.line_origin = origin;
        self
    }

    /// Returns the recorded origin of a new line.
    #[must_use]
    pub const fn line_origin(&self) -> &LineOrigin {
        &self.line_origin
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
