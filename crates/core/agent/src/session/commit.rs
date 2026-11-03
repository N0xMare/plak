use super::{
    BranchHead, BranchLease, CheckpointObjectBundle, CheckpointObjectRef, Digest, FeedFact,
    FeedSeq, StoreAdmission,
};

/// Exact fenced branch-head CAS request for one immutable checkpoint.
#[derive(Clone, Debug)]
pub struct CommitCheckpoint {
    lease: BranchLease,
    expected: BranchHead,
    checkpoint: CheckpointObjectBundle,
    admission: StoreAdmission,
    feed_facts: Vec<FeedFact>,
    consumed_line_terminals: Vec<u32>,
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
            consumed_line_terminals: Vec::new(),
        }
    }

    /// Deletes these jobs' `line_job_terminals` rows of the checkpoint's line
    /// and generation in the same transaction: this checkpoint folds them
    /// into its job ledger. Rows already gone are not an error.
    #[must_use]
    pub fn with_consumed_line_terminals(mut self, jobs: Vec<u32>) -> Self {
        self.consumed_line_terminals = jobs;
        self
    }

    /// Returns the line terminals this commit consumes.
    #[must_use]
    pub fn consumed_line_terminals(&self) -> &[u32] {
        &self.consumed_line_terminals
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
