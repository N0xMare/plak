//! Count-based bounds for host read batches.
use super::*;
use crate::sessions::feed::FeedTerminalOrigin;
use plakous_agent::internal as store_api;

struct CountingReads {
    inner: MemorySessionStore,
    branches: AtomicUsize,
    last_branch_operation: Mutex<Option<&'static str>>,
    last_metadata_operation: Mutex<Option<&'static str>>,
    checkpoints: AtomicUsize,
    receipts: AtomicUsize,
    pruned: AtomicBool,
    lease: Mutex<Option<plakous_agent::internal::BranchLease>>,
}

impl CountingReads {
    fn new() -> TestResult<Self> {
        Ok(Self {
            inner: MemorySessionStore::new(MemoryStoreLimits::default())?,
            branches: AtomicUsize::new(0),
            last_branch_operation: Mutex::new(None),
            last_metadata_operation: Mutex::new(None),
            checkpoints: AtomicUsize::new(0),
            receipts: AtomicUsize::new(0),
            pruned: AtomicBool::new(false),
            lease: Mutex::new(None),
        })
    }

    fn reset(&self) {
        self.branches.store(0, Ordering::SeqCst);
        *test_lock(&self.last_branch_operation) = None;
        *test_lock(&self.last_metadata_operation) = None;
        self.checkpoints.store(0, Ordering::SeqCst);
        self.receipts.store(0, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn line_identity_does_not_load_conversation_checkpoints() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(CountingReads::new()?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let locator = completed_session(&sessions, &workspace, "line-read").await?;
    store.reset();
    let head = sessions.line_head_of(locator).await?;
    assert_eq!(head.session_id(), locator.session_id());
    assert_eq!(head.branch_id(), locator.branch_id());
    assert_eq!(store.branches.load(Ordering::SeqCst), 0);
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn historical_child_retirement_head_must_have_published_by_the_cursor() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(CountingReads::new()?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let locator = completed_session(&sessions, &workspace, "retirement-head").await?;
    let page = sessions
        .feed_page(FeedPageRequest::after(FeedCursor::start(
            locator.session_id(),
        )))
        .await?;
    let FeedPage::Records { records, tail, .. } = page else {
        return Err("expected the entire fixture feed".into());
    };
    let published = records
        .iter()
        .find_map(|record| match record.header().change {
            FeedChange::Advanced { checkpoint, .. } if checkpoint == locator.checkpoint_id() => {
                Some(record.header().seq.get())
            }
            _ => None,
        })
        .ok_or("missing head publication")?;
    assert!(
        published < tail.get(),
        "lease release follows head publication"
    );
    store.reset();
    assert!(
        !sessions
            .quiescent_head_at_feed_cursor(locator, published - 1)
            .await?
    );
    assert!(
        sessions
            .quiescent_head_at_feed_cursor(locator, published)
            .await?
    );
    assert!(
        sessions
            .quiescent_head_at_feed_cursor(locator, tail.get())
            .await?
    );
    assert!(
        !sessions
            .quiescent_head_at_feed_cursor(locator, tail.get() + 1)
            .await?
    );
    assert_eq!(store.branches.load(Ordering::SeqCst), 0);
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn head_observations_do_not_request_materialized_branches() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(CountingReads::new()?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let locator = completed_session(&sessions, &workspace, "head-metadata").await?;
    store.reset();

    assert_eq!(sessions.head_running_turn(locator).await?, None);
    assert_eq!(
        *test_lock(&store.last_metadata_operation),
        Some("retention_seed")
    );
    let report = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("context:head-metadata")?,
            locator,
            ContextQuery::Current,
        ))
        .await?;
    assert!(matches!(
        report.kind(),
        SessionContextStatusKind::Unavailable(_)
    ));
    assert_eq!(
        *test_lock(&store.last_metadata_operation),
        Some("detached_context_status")
    );

    let stale = SessionLocator::new(
        locator.session_id(),
        locator.branch_id(),
        CheckpointId::try_from(Uuid::from_u128(1))?,
    );
    let error = sessions
        .head_running_turn(stale)
        .await
        .expect_err("stale head");
    assert_eq!(error.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(error.current_locator(), Some(locator));
    assert_eq!(store.branches.load(Ordering::SeqCst), 0);
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 0);
    assert!(sessions.head_is_quiescent(locator).await?);
    assert_eq!(store.branches.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn facade_placeholder_suffix_is_refused_before_any_receipt_read() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(CountingReads::new()?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let source = SessionLocator::new(
        SessionId::try_from(Uuid::from_u128(1))?,
        BranchId::try_from(Uuid::from_u128(2))?,
        CheckpointId::try_from(Uuid::from_u128(3))?,
    );
    for reason in [CompactReason::Manual, CompactReason::Scheduled] {
        let error = sessions
            .compact(CompactSessionRequest::new(
                SessionOperationId::new("x:placeholders")?,
                source,
                reason,
                runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"suffix-read-probe")?,
            ))
            .await
            .expect_err("the facade must refuse the reserved suffix itself");
        assert_eq!(error.kind(), AgentSessionsErrorKind::InvalidInput);
        assert!(error.to_string().contains(":placeholders"));
    }
    assert_eq!(store.receipts.load(Ordering::SeqCst), 0);
    assert_eq!(store.branches.load(Ordering::SeqCst), 0);
    assert_eq!(store.checkpoints.load(Ordering::SeqCst), 0);
    Ok(())
}

impl SessionStore for CountingReads {
    plakous_test_support::session_store_methods! {
        forward store_api, inner;
        lease_now, recover_create, release, commit, fork, recover_fork, list,
        usage, archive, delete, prune, recover_delete, read_feed, feed_tail, branch_line,
        load_line, line_job_terminals, record_line_job_terminals, flush, shutdown,
    }

    fn acquire(
        &self,
        request: AcquireBranch,
    ) -> store_api::StoreFuture<'_, store_api::BranchLease> {
        Box::pin(async move {
            let result = self.inner.acquire(request).await;
            if let Ok(lease) = &result {
                *test_lock(&self.lease) = Some(lease.clone());
            }
            result
        })
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

    fn load_branch(
        &self,
        session: SessionRef,
        operation: &'static str,
    ) -> plakous_agent::internal::StoreFuture<'_, LoadedBranch> {
        self.branches.fetch_add(1, Ordering::SeqCst);
        *test_lock(&self.last_branch_operation) = Some(operation);
        self.inner.load_branch(session, operation)
    }

    fn load_branch_metadata(
        &self,
        session: SessionRef,
        operation: &'static str,
    ) -> store_api::StoreFuture<
        '_,
        (
            store_api::BranchHead,
            store_api::RuntimeCheckpointInspection,
        ),
    > {
        *test_lock(&self.last_metadata_operation) = Some(operation);
        self.inner.load_branch_metadata(session, operation)
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

    fn recover_commit(
        &self,
        checkpoint: CheckpointId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CommittedCheckpoint>
    {
        self.receipts.fetch_add(1, Ordering::SeqCst);
        self.inner.recover_commit(checkpoint)
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
    store.pruned.store(true, Ordering::SeqCst);
    for (origin, operation) in [
        (None, "feed_terminals_fallback"),
        (
            Some(FeedTerminalOrigin::TreeProjection),
            "tree_publication_fallback",
        ),
        (Some(FeedTerminalOrigin::WakeAdmission), "wake_admission"),
    ] {
        store.reset();
        let requests = vec![request, older, request, older];
        let outcomes = match origin {
            None => sessions.feed_terminals(requests).await,
            Some(origin) => sessions.feed_terminals_from(requests, origin).await,
        };
        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, Ok(FeedTerminalOutcome::Terminal(_))))
        );
        assert_eq!(store.checkpoints.load(Ordering::SeqCst), 2);
        assert_eq!(store.branches.load(Ordering::SeqCst), 1);
        assert_eq!(*test_lock(&store.last_branch_operation), Some(operation));
    }
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
    assert_eq!(
        sessions.committed_calls(locator, Vec::new()).await?,
        [] as [(CallId, bool); 0]
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
    let loaded = store
        .inner
        .load_branch(locator.session_ref(), "test")
        .await?;
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
