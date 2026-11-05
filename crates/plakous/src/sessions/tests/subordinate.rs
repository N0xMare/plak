//! A subordinate handle over a store whose usage the test sets: where its
//! ordinary writes stop, that it reclaims once before refusing, and that
//! creates, forks and replays meet the mark exactly as the class rules say.

use super::*;
use plakous_agent::internal::{
    ArchiveReceipt, ArchiveSession, BranchLease, CheckpointId, CommitReceipt, CommittedCheckpoint,
    CreateReceipt, DeleteReceipt, DeleteSession, DurabilityReport, FeedPage, FeedSeq, FeedTail,
    ForkBranch, ForkReceipt, LineJobTerminal, LineRecord, LineTerminalsReceipt, LoadedBranch, Page,
    PruneBranch, PruneReceipt, RecordLineJobTerminals, RecoverFork, SessionQuery, SessionRef,
    SessionStoreError, SessionSummary, StoreAdmission, StoreFuture, StoreReclaimMode,
    StoreReclaimReport, StoreUsage, StoredCheckpoint,
};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicU64;

/// The checkpoint limit the test reports: the store's own ordinary threshold
/// is 56 and a subordinate handle's mark 48.
const LIMIT: u64 = 64;
const MARK: u64 = 48;

/// A memory store whose checkpoint usage reads `used`; a reclaim frees one
/// checkpoint when `reclaim_frees` is set.
struct Metered {
    inner: MemorySessionStore,
    used: AtomicU64,
    reclaims: AtomicUsize,
    reclaim_frees: AtomicBool,
}

impl SessionStore for Metered {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        self.inner.lease_now()
    }
    fn create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        self.inner.create(request)
    }
    fn recover_create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        self.inner.recover_create(request)
    }
    fn load_branch(&self, session: SessionRef) -> StoreFuture<'_, LoadedBranch> {
        self.inner.load_branch(session)
    }
    fn load_checkpoint(&self, checkpoint: CheckpointId) -> StoreFuture<'_, StoredCheckpoint> {
        self.inner.load_checkpoint(checkpoint)
    }
    fn acquire(&self, request: AcquireBranch) -> StoreFuture<'_, BranchLease> {
        self.inner.acquire(request)
    }
    fn renew(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        self.inner.renew(lease)
    }
    fn release(&self, lease: BranchLease) -> StoreFuture<'_, ()> {
        self.inner.release(lease)
    }
    fn commit(&self, request: CommitCheckpoint) -> StoreFuture<'_, CommitReceipt> {
        self.inner.commit(request)
    }
    fn recover_commit(&self, checkpoint: CheckpointId) -> StoreFuture<'_, CommittedCheckpoint> {
        self.inner.recover_commit(checkpoint)
    }
    fn fork(&self, request: ForkBranch) -> StoreFuture<'_, ForkReceipt> {
        self.inner.fork(request)
    }
    fn recover_fork(&self, request: RecoverFork) -> StoreFuture<'_, ForkReceipt> {
        self.inner.recover_fork(request)
    }
    fn list(&self, query: SessionQuery) -> StoreFuture<'_, Page<SessionSummary>> {
        self.inner.list(query)
    }
    fn reclaim(&self, mode: StoreReclaimMode) -> StoreFuture<'_, StoreReclaimReport> {
        assert_eq!(mode, StoreReclaimMode::Bounded);
        self.reclaims.fetch_add(1, Ordering::SeqCst);
        if self.reclaim_frees.load(Ordering::SeqCst) {
            self.used.fetch_sub(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(StoreReclaimReport::default()) })
    }
    fn usage(&self) -> StoreFuture<'_, StoreUsage> {
        let used = self.used.load(Ordering::SeqCst);
        Box::pin(async move { Ok(StoreUsage::new((used, LIMIT), (0, 1 << 20), (0, 1 << 30))) })
    }
    fn archive(&self, request: ArchiveSession) -> StoreFuture<'_, ArchiveReceipt> {
        self.inner.archive(request)
    }
    fn delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        self.inner.delete(request)
    }
    fn prune(&self, request: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
        self.inner.prune(request)
    }
    fn recover_delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        self.inner.recover_delete(request)
    }
    fn read_feed(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> StoreFuture<'_, FeedPage> {
        self.inner.read_feed(session_id, after, max_records)
    }
    fn feed_tail(&self, session_id: SessionId) -> StoreFuture<'_, FeedTail> {
        self.inner.feed_tail(session_id)
    }
    fn load_line(&self, line: plakous_model::LineId) -> StoreFuture<'_, LineRecord> {
        self.inner.load_line(line)
    }
    fn line_job_terminals(
        &self,
        line: plakous_model::LineId,
    ) -> StoreFuture<'_, Vec<LineJobTerminal>> {
        self.inner.line_job_terminals(line)
    }
    fn record_line_job_terminals(
        &self,
        request: RecordLineJobTerminals,
    ) -> StoreFuture<'_, LineTerminalsReceipt> {
        self.inner.record_line_job_terminals(request)
    }
    fn flush(&self) -> StoreFuture<'_, DurabilityReport> {
        self.inner.flush()
    }
    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
        self.inner.shutdown()
    }
}

async fn create(
    sessions: &AgentSessions,
    workspace: &Arc<WorkspacePolicy>,
    operation: &str,
) -> TestResult<Result<SessionLocator, AgentSessionsError>> {
    let request = CreateSessionRequest::new(
        SessionOperationId::new(operation)?,
        completing_runtime_binding_for(
            Arc::clone(workspace),
            descriptor()?,
            b"subordinate-account",
        )?,
    );
    Ok(match sessions.create(request).await {
        Ok(CreateSessionOutcome::Created { session, .. }) => session.release().await,
        Err(error) => Err(error),
    })
}

fn assert_held_at_the_mark(refused: Result<impl fmt::Debug, AgentSessionsError>) -> TestResult {
    let error = refused.err().ok_or("new work passed the mark")?;
    assert_eq!(
        error.kind(),
        AgentSessionsErrorKind::StoragePressure,
        "{error}"
    );
    let pressure = error.storage_pressure().ok_or("untyped refusal")?;
    assert_eq!(pressure.admission(), SessionStoreAdmission::Ordinary);
    assert_eq!(pressure.threshold(), MARK);
    assert_eq!(pressure.limit(), LIMIT);
    assert!(pressure.subordinate());
    assert!(
        pressure.to_string().contains(&format!(
            "subordinate ordinary writes stop at {MARK}, before the store's 56"
        )),
        "{pressure}"
    );
    Ok(())
}

#[tokio::test]
async fn subordinate_writes_stop_at_the_mark_after_one_reclaim_and_replays_still_resolve()
-> TestResult {
    assert_eq!(backend::subordinate_threshold(LIMIT), MARK);
    assert_eq!(StoreAdmission::Ordinary.threshold(LIMIT), 56);
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(Metered {
        inner: MemorySessionStore::with_clock(
            MemoryStoreLimits::default(),
            Arc::new(FixedLeaseClock::new(timestamp()?)),
        )?,
        used: AtomicU64::new(0),
        reclaims: AtomicUsize::new(0),
        reclaim_frees: AtomicBool::new(false),
    });
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let children = sessions.with_subordinate_admission();
    let below = create(&children, &workspace, "subordinate:below-the-mark").await??;
    assert_eq!(
        store.reclaims.load(Ordering::SeqCst),
        0,
        "no reclaim below the mark"
    );

    // The handle reports its mark as the ordinary threshold; the store's own
    // reading through the plain handle keeps the class threshold.
    store.used.store(MARK, Ordering::SeqCst);
    let held = children.store.usage().await?;
    assert_eq!(held.bounds()[0].ordinary_threshold(), MARK);
    assert!(held.ordinary_exhausted().is_some());
    let plain = sessions.store.usage().await?;
    assert_eq!(plain.bounds()[0].ordinary_threshold(), 56);
    assert!(plain.ordinary_exhausted().is_none());
    assert_eq!(children.store_usage().await?[0].ordinary_threshold(), 56);

    // A create at the mark is refused after one bounded reclaim; the plain
    // handle still creates.
    assert_held_at_the_mark(create(&children, &workspace, "subordinate:at-the-mark").await?)?;
    assert_eq!(store.reclaims.load(Ordering::SeqCst), 1);
    create(&sessions, &workspace, "subordinate:root-at-the-mark").await??;

    // A fork is new work too.
    let fork = children
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("subordinate:fork")?,
            below,
        ))
        .await;
    assert_held_at_the_mark(fork)?;
    let ForkSessionOutcome::Forked { .. } = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("subordinate:root-fork")?,
            below,
        ))
        .await?
    else {
        return Err("the plain handle's fork did not publish".into());
    };

    // A create the store already accepted replays to its receipt past the mark.
    create(&children, &workspace, "subordinate:below-the-mark").await??;

    // When the reclaim brings usage back under the mark, the write proceeds.
    store.reclaim_frees.store(true, Ordering::SeqCst);
    let reclaims = store.reclaims.load(Ordering::SeqCst);
    create(&children, &workspace, "subordinate:after-reclaim").await??;
    assert_eq!(store.reclaims.load(Ordering::SeqCst), reclaims + 1);
    assert_eq!(store.used.load(Ordering::SeqCst), MARK - 1);
    Ok(())
}
