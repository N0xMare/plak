//! Count-based bounds for host read batches.
use super::*;

struct CountingReads {
    inner: MemorySessionStore,
    branches: AtomicUsize,
    checkpoints: AtomicUsize,
    pruned: AtomicBool,
    lease: Mutex<Option<plakous_agent::internal::BranchLease>>,
}

impl CountingReads {
    fn new() -> TestResult<Self> {
        Ok(Self {
            inner: MemorySessionStore::new(MemoryStoreLimits::default())?,
            branches: AtomicUsize::new(0),
            checkpoints: AtomicUsize::new(0),
            pruned: AtomicBool::new(false),
            lease: Mutex::new(None),
        })
    }

    fn reset(&self) {
        self.branches.store(0, Ordering::SeqCst);
        self.checkpoints.store(0, Ordering::SeqCst);
    }
}

impl SessionStore for CountingReads {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        self.inner.lease_now()
    }

    fn create(
        &self,
        request: CreateSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CreateReceipt> {
        Box::pin(async move {
            let result = self.inner.create(request).await;
            if let Ok(receipt) = &result {
                *test_lock(&self.lease) = Some(receipt.lease().clone());
            }
            result
        })
    }

    fn recover_create(
        &self,
        request: CreateSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CreateReceipt> {
        self.inner.recover_create(request)
    }

    fn load_branch(
        &self,
        session: SessionRef,
    ) -> plakous_agent::internal::StoreFuture<'_, LoadedBranch> {
        self.branches.fetch_add(1, Ordering::SeqCst);
        self.inner.load_branch(session)
    }

    fn load_checkpoint(
        &self,
        checkpoint: CheckpointId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::StoredCheckpoint> {
        self.checkpoints.fetch_add(1, Ordering::SeqCst);
        if self.pruned.load(Ordering::SeqCst) {
            return Box::pin(async { Err(SessionStoreError::Pruned) });
        }
        self.inner.load_checkpoint(checkpoint)
    }

    fn acquire(
        &self,
        request: AcquireBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::BranchLease> {
        self.inner.acquire(request)
    }

    fn renew(
        &self,
        lease: plakous_agent::internal::BranchLease,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::BranchLease> {
        Box::pin(async move {
            let result = self.inner.renew(lease).await;
            if let Ok(lease) = &result {
                *test_lock(&self.lease) = Some(lease.clone());
            }
            result
        })
    }

    fn release(
        &self,
        lease: plakous_agent::internal::BranchLease,
    ) -> plakous_agent::internal::StoreFuture<'_, ()> {
        self.inner.release(lease)
    }

    fn commit(
        &self,
        request: CommitCheckpoint,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CommitReceipt> {
        self.inner.commit(request)
    }

    fn recover_commit(
        &self,
        checkpoint: CheckpointId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CommittedCheckpoint>
    {
        self.inner.recover_commit(checkpoint)
    }

    fn fork(
        &self,
        request: plakous_agent::internal::ForkBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ForkReceipt> {
        self.inner.fork(request)
    }

    fn recover_fork(
        &self,
        request: plakous_agent::internal::RecoverFork,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ForkReceipt> {
        self.inner.recover_fork(request)
    }

    fn list(
        &self,
        query: plakous_agent::internal::SessionQuery,
    ) -> plakous_agent::internal::StoreFuture<
        '_,
        plakous_agent::internal::Page<plakous_agent::internal::SessionSummary>,
    > {
        self.inner.list(query)
    }

    fn usage(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::StoreUsage> {
        self.inner.usage()
    }

    fn archive(
        &self,
        request: ArchiveSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ArchiveReceipt> {
        self.inner.archive(request)
    }

    fn delete(
        &self,
        request: DeleteSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DeleteReceipt> {
        self.inner.delete(request)
    }

    fn prune(
        &self,
        request: plakous_agent::internal::PruneBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::PruneReceipt> {
        self.inner.prune(request)
    }

    fn recover_delete(
        &self,
        request: DeleteSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DeleteReceipt> {
        self.inner.recover_delete(request)
    }

    fn read_feed(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::FeedPage> {
        self.inner.read_feed(session_id, after, max_records)
    }

    fn feed_tail(
        &self,
        session_id: SessionId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::FeedTail> {
        self.inner.feed_tail(session_id)
    }

    fn flush(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.flush()
    }

    fn shutdown(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.shutdown()
    }
}

#[tokio::test]
// Terminal content reads only; a BehindRetention seed may add one head load.
async fn terminal_batch_loads_each_checkpoint_and_pruned_head_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(CountingReads::new()?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let locator = completed_session(&sessions, &workspace, "batch").await?;
    let records = read_all(&sessions, locator.session_id()).await?;
    let terminal = records
        .iter()
        .find_map(|record| {
            record.facts().iter().find_map(|fact| {
                if let FeedFact::TurnTerminal { turn_id, .. } = fact {
                    Some((record, *turn_id))
                } else {
                    None
                }
            })
        })
        .ok_or("terminal")?;
    let request = FeedTerminalRequest {
        session: locator.session_id(),
        branch: locator.branch_id(),
        checkpoint: checkpoint_of(terminal.0)?,
        turn_id: terminal.1,
    };
    store.reset();
    let outcomes = sessions.feed_terminals(vec![request; 20]).await;
    assert_eq!(outcomes.len(), 20);
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, Ok(FeedTerminalOutcome::Terminal(_))))
    );
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(store.branches.load(Ordering::SeqCst), 0);
    store.reset();
    let foreign = FeedTerminalRequest {
        branch: BranchId::try_from(Uuid::from_u128(0xdead))?,
        ..request
    };
    let outcomes = sessions.feed_terminals(vec![foreign, request]).await;
    assert_eq!(
        outcomes[0].as_ref().err().map(AgentSessionsError::kind),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    assert!(matches!(outcomes[1], Ok(FeedTerminalOutcome::Terminal(_))));
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 1);
    let mut older = request;
    older.checkpoint = checkpoint_of(&records[0])?;
    store.reset();
    store.pruned.store(true, Ordering::SeqCst);
    let outcomes = sessions
        .feed_terminals(vec![request, older, request, older])
        .await;
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, Ok(FeedTerminalOutcome::Terminal(_))))
    );
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 2);
    assert_eq!(store.branches.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn committed_call_batch_authenticates_one_exact_head_for_many_ids() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(CountingReads::new()?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let account = b"carry-account";
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(
            CreateSessionRequest::new(
                SessionOperationId::new("create:wait-batch")?,
                completing_runtime_binding_for(workspace, descriptor()?, account)?,
            )
            .with_history(PortableSessionHistory::new(
                carried_segments()?,
                account_reference(account)?,
                ModelId::new("test.model")?,
            )),
        )
        .await?;
    let locator = session.locator();
    let present = CallId::try_from(Uuid::from_u128(0xca11))?;
    let absent = CallId::try_from(Uuid::from_u128(0xdead))?;
    let mut ids = vec![present; 200];
    ids.push(absent);
    store.reset();
    let evidence = sessions.committed_calls(locator, ids).await?;
    assert_eq!(evidence.len(), 201);
    assert!(
        evidence[..200]
            .iter()
            .all(|entry| *entry == (present, true))
    );
    assert_eq!(evidence[200], (absent, false));
    assert_eq!(store.branches.load(Ordering::SeqCst), 1);
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 0);
    store.reset();
    assert!(
        sessions
            .committed_calls(locator, Vec::new())
            .await?
            .is_empty()
    );
    assert_eq!(store.branches.load(Ordering::SeqCst), 1);
    session
        .agent()
        .prompt("move the head")
        .await?
        .result()
        .await?;
    store.reset();
    assert_eq!(
        sessions
            .committed_calls(locator, vec![present])
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::StaleLocator)
    );
    assert_eq!(store.branches.load(Ordering::SeqCst), 1);
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn prune_keeps_published_head_and_parent_without_store_loads() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(CountingReads::new()?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:cache-prune")?,
            completing_runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?;
    for _ in 0..2 {
        session.agent().prompt("one turn").await?.result().await?;
    }
    let locator = session.locator();
    let loaded = store.inner.load_branch(locator.session_ref()).await?;
    let parent = loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .topology()
        .parent_checkpoint_id
        .ok_or("parent")?;
    let records = read_all(&sessions, locator.session_id()).await?;
    let ancestor = records
        .iter()
        .filter_map(|record| checkpoint_of(record).ok())
        .find(|checkpoint| {
            sessions
                .feed
                .published_checkpoint(locator.session_id(), *checkpoint)
                .is_some_and(|cached| {
                    cached.manifest().topology().branch_revision
                        < loaded.head().revision().get() - 1
                })
        })
        .ok_or("cached ancestor below the prospective horizon")?;
    let turn_id = records
        .iter()
        .rev()
        .find_map(|record| {
            record.facts().iter().find_map(|fact| {
                if let FeedFact::TurnTerminal { turn_id, .. } = fact {
                    Some(*turn_id)
                } else {
                    None
                }
            })
        })
        .ok_or("turn")?;
    let lease = test_lock(&store.lease).clone().ok_or("lease")?;
    let receipt = sessions
        .store
        .prune(plakous_agent::internal::PruneBranch::new(
            lease,
            loaded.head(),
            std::collections::BTreeSet::from([parent]),
            1,
        ))
        .await?;
    assert!(receipt.pruned_checkpoints() > 0);
    assert!(
        sessions
            .feed
            .published_checkpoint(locator.session_id(), ancestor)
            .is_none()
    );
    assert!(matches!(
        store.inner.load_checkpoint(ancestor).await,
        Err(SessionStoreError::Pruned)
    ));
    store.reset();
    assert!(matches!(
        sessions
            .feed_terminal(FeedTerminalRequest {
                session: locator.session_id(),
                branch: locator.branch_id(),
                checkpoint: locator.checkpoint_id(),
                turn_id,
            })
            .await?,
        FeedTerminalOutcome::Terminal(_)
    ));
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 0);
    assert_eq!(store.branches.load(Ordering::SeqCst), 0);
    assert!(
        sessions
            .feed
            .published_checkpoint(locator.session_id(), parent)
            .is_some()
    );
    session.release().await?;
    assert!(
        sessions
            .feed
            .published_checkpoint(locator.session_id(), locator.checkpoint_id())
            .is_none()
    );
    Ok(())
}
