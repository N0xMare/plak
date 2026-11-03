use super::{
    AcquireBranch, ArchiveReceipt, ArchiveSession, BranchLease, BranchRevision, CheckpointId,
    CommitCheckpoint, CommitReceipt, CommittedCheckpoint, CreateReceipt, CreateSession,
    DeleteReceipt, DeleteSession, DurabilityReport, FeedPage, FeedSeq, FeedTail, ForkBranch,
    ForkReceipt, Future, LeaseInstant, LineRecord, LineTerminalsReceipt, LoadedBranch,
    NonZeroUsize, Page, Pin, PruneBranch, PruneReceipt, RecordLineJobTerminals, RecoverFork,
    SessionId, SessionQuery, SessionRef, SessionStoreError, SessionSummary, StoreReclaimMode,
    StoreReclaimReport, StoreUsage, StoredCheckpoint,
};
use crate::manifest::LineJobTerminal;
use plakous_model::LineId;

/// Boxed future returned by object-safe store operations.
pub type StoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SessionStoreError>> + Send + 'a>>;

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
    /// Retry capacity-blocked colocated stores and the backend's own writer.
    /// This is not permission to bypass a branch lease or checkpoint CAS.
    fn recover_storage(&self) -> StoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Whether an authoritative write most recently refused for exhausted
    /// storage. This process-local observation is not lease or commit authority.
    fn storage_unavailable(&self) -> bool {
        false
    }

    /// Whether the store's owning host requested shutdown while its colocated
    /// authoritative stores were unavailable. Shared backends must not bind a
    /// single agent's interrupt here; bind it through that agent's control.
    /// This observation carries no lease authority.
    fn storage_wait_cancelled(&self) -> bool {
        false
    }

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
    /// A genesis at line generation 1 starts its own line; a genesis at a
    /// higher generation advances that line's head across sessions under the
    /// same rules as a fork's advance.
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

    /// Releases a set of exact leases, all or none: on error no lease in the batch was
    /// released, so a caller may treat every lease as still held.
    ///
    /// The default releases one lease at a time and keeps that contract only for a batch of
    /// one; a backend that can be handed several leases must override it (the bundled memory
    /// and SQLite stores do).
    fn release_many(&self, leases: Vec<BranchLease>) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            for lease in leases {
                self.release(lease).await?;
            }
            Ok(())
        })
    }

    /// CAS-publishes one exact immutable checkpoint under the current lease.
    /// Inside the same transaction the branch must be its line's head at the
    /// checkpoint's line generation, or the commit is refused with
    /// `NotLineHead`. `Corrupt`, `StorageFull`, `Invalid`, and `Capacity` must only be returned
    /// when nothing was published. If publication cannot be ruled out, return
    /// `BackendUnavailable` so the caller reconciles the exact acknowledgement.
    fn commit(&self, request: CommitCheckpoint) -> StoreFuture<'_, CommitReceipt>;

    /// Loads the exact accepted commit operation and stable receipt.
    fn recover_commit(&self, checkpoint: CheckpointId) -> StoreFuture<'_, CommittedCheckpoint>;

    /// Atomically creates one independently fenced branch from an exact
    /// quiescent source checkpoint. The target's topology names its line
    /// ([`super::LineStep::of`]): a new line records the source as its
    /// origin; an advance moves the source line's head to the target and
    /// adds one to its generation in the same transaction, refused with
    /// `NotLineHead` unless the source is still the head at the step's
    /// generation and with `Conflict` while the line has unconsumed job
    /// terminals.
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

    /// Reads one line's head without a lease or a write. A line whose head
    /// session was deleted is `NotFound`.
    fn load_line(&self, line: LineId) -> StoreFuture<'_, LineRecord>;

    /// Reads the recorded, unconsumed job terminals of one line at its
    /// current generation, ordered by job number, without a lease.
    fn line_job_terminals(&self, line: LineId) -> StoreFuture<'_, Vec<LineJobTerminal>>;

    /// Records job terminals for a line head under that head's exact lease,
    /// fenced like a commit: the lease must be live and the branch must be the
    /// line's head at `generation`, or nothing is written
    /// ([`SessionStoreError::NotLineHead`]). A job already recorded keeps its
    /// first evidence. Newly recorded jobs append one `LineJobTerminals`
    /// record to the head session's feed in the same transaction; a write that
    /// adds nothing appends nothing.
    fn record_line_job_terminals(
        &self,
        request: RecordLineJobTerminals,
    ) -> StoreFuture<'_, LineTerminalsReceipt>;

    /// Waits behind all accepted work and truthfully reports durability debt.
    fn flush(&self) -> StoreFuture<'_, DurabilityReport>;

    /// Stops mutation and completes the terminal durability barrier.
    ///
    /// Exact recovery reads and repeated shutdown remain available through the
    /// selected backend owner; final handle drop joins its remaining resources.
    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport>;
}
