#![allow(
    clippy::too_many_lines,
    reason = "Credential-wait recovery scenarios exercise cancellation and fresh authority before any resumed provider dispatch"
)]

use super::*;
use crate::RuntimeCheckpointKind;
use plakous_model::RuntimeModelAuthorityError;

struct WaitingModel {
    inner: Arc<ArtifactModel>,
    prepared: Mutex<Option<(ValidatedRequestPlan, PreparedCallRecord)>>,
}

impl LanguageModel for WaitingModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            if lock_mutex(&self.prepared).is_some() {
                return Err(RuntimeModelAuthorityError::AwaitingCredential);
            }
            Arc::clone(&self.inner)
                .revalidate_runtime_authority(checked_at)
                .await
        })
    }

    fn wait_for_runtime_authority(
        self: Arc<Self>,
    ) -> plakous_model::RuntimeModelAuthorityWaitOperation {
        Box::pin(std::future::pending())
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let prepared = Arc::clone(&self.inner).prepare_call(plan.clone(), context)?;
        *lock_mutex(&self.prepared) = Some((plan, prepared.record().clone()));
        Ok(prepared)
    }
}

fn first_dispatch_authority_rules(
    record: &PreparedCallRecord,
    plan: &ValidatedRequestPlan,
    descriptor: &ResolvedModelDescriptor,
) -> TestResult {
    let now = Timestamp::new(21, 0)?;
    let original = record.authority_evidence();
    let fresh = |generation, observed_at, account| {
        RuntimeModelAuthority::new(
            descriptor,
            account,
            original.credential_authority_binding(),
            generation,
            Timestamp::new(observed_at, 0)?,
            Some(Timestamp::new(30, 0)?),
        )
        .map_err(Box::<dyn std::error::Error>::from)
    };
    let renewed = fresh(3, 21, original.account_binding())?;
    assert!(
        record.revalidate(descriptor, plan, now).is_err(),
        "historical expired authority is never fresh authority"
    );
    record.revalidate_first_dispatch(descriptor, plan, &renewed, now)?;
    assert!(
        record
            .revalidate_selected_authority(&renewed, descriptor, now)
            .is_err(),
        "Working recovery retains strict authority equality"
    );
    for authority in [
        fresh(1, 21, original.account_binding())?,
        fresh(3, 8, original.account_binding())?,
        fresh(
            3,
            21,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"another-account"])?,
        )?,
    ] {
        assert!(
            record
                .revalidate_first_dispatch(descriptor, plan, &authority, now)
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn crash_while_awaiting_credentials_recovers_prepared_operation_without_dispatch()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let descriptor = durable_test_descriptor("test.route")?;
    let inner = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 2)?,
        0,
        91_000,
        false,
    ));
    let waiting = Arc::new(WaitingModel {
        inner: Arc::clone(&inner),
        prepared: Mutex::new(None),
    });
    let mut builder = durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&inner));
    builder.model = waiting.clone();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create("create:credential-wait-crash", builder)
        .await?;
    let command = CommandId::new("start:credential-wait-crash")?;
    let mut turn = agent
        .start(
            command.clone(),
            "keep this request undispatched until login",
        )
        .await?
        .into_turn()
        .ok_or("new turn")?;
    let mut events = turn.take_events()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await?.kind(),
                AgentEventKind::ModelCredentialWait { waiting: true }
            ) {
                return Ok::<_, crate::TurnEventError>(());
            }
        }
    })
    .await??;
    let head = agent.checkpoint();
    // This is the same per-session gate the pre-send fence holds, not merely
    // a separate request-slot budget. A waiter must relinquish it.
    let gate = Arc::clone(&agent.inner.provider_dispatch_gate);
    // A liveness bound only: generous, so load never trips it.
    let guard = tokio::time::timeout(Duration::from_secs(30), gate.lock()).await?;
    drop(guard);
    let providers: Vec<_> = head
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .collect();
    assert_eq!(providers.len(), 1);
    assert!(matches!(
        providers[0].state(),
        DurableOperationState::Prepared
    ));
    let durable_record: PreparedCallRecord =
        serde_json::from_slice(providers[0].request().bytes())?;
    let (plan, captured) = lock_mutex(&waiting.prepared)
        .clone()
        .ok_or("missing captured request")?;
    assert_eq!(durable_record, captured);
    first_dispatch_authority_rules(&captured, &plan, &descriptor)?;
    assert_eq!(inner.calls.load(Ordering::Acquire), 0);

    // Retain exactly the durable prefix, then lose the owner without running
    // terminal publication, as the existing crash fixtures do for SIGKILL.
    let crashed = Arc::new(TestCreateStore::default());
    *lock_mutex(&crashed.state) = lock_mutex(&store.state).clone();
    *lock_mutex(&crashed.genesis) = lock_mutex(&store.genesis).clone();
    *lock_mutex(&crashed.feed_records) = lock_mutex(&store.feed_records).clone();
    crashed.feed_retained_from.store(
        store.feed_retained_from.load(Ordering::Acquire),
        Ordering::Release,
    );
    lock_mutex(&crashed.state).active_lease = None;
    agent.begin_abandon();
    agent.abort_driver_for_test();
    drop(agent);
    drop(turn);
    let sessions = DurableSessionRuntime::current(
        crashed.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let fresh = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 2)?,
        0,
        92_000,
        false,
    ));
    let address = DurableSessionAddress::new(
        head.manifest().topology().session_id,
        head.manifest().topology().branch_id,
        head.id(),
    );
    let opened = sessions
        .open(
            "open:credential-wait-crash",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&fresh)),
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("undispatched credential wait became recovery-only".into());
    };
    assert_eq!(fresh.calls.load(Ordering::Acquire), 0);
    assert_eq!(opened.checkpoint().kind(), RuntimeCheckpointKind::Quiescent);
    let report = opened.abandoned_turn().ok_or("recovery report")?;
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::ModelRequest);
    let checkpoint = opened.checkpoint();
    assert!(crate::runtime::checkpoint_root_terminal_result(&checkpoint, &command)?.is_some());
    assert_eq!(
        checkpoint
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter_map(|operation| {
                let DurableOperationState::Finalized(evidence) = operation.state() else {
                    return None;
                };
                read_terminal_result_record(evidence.bytes()).ok()
            })
            .count(),
        1,
        "one terminal result record, not just a quiescent head"
    );
    assert_eq!(
        lock_mutex(&crashed.accepted_commits)
            .iter()
            .filter(
                |(checkpoint, _)| crate::runtime::checkpoint_root_terminal_result(
                    checkpoint, &command
                )
                .is_ok_and(|result| result.is_some())
            )
            .count(),
        1
    );
    opened.shutdown().await?;
    Ok(())
}

struct WaitingCompaction {
    inner: Arc<RecoveryCompactionModel>,
    ready: AtomicBool,
    release: Notify,
    park_after_prepare: bool,
    prepared: AtomicBool,
}

impl LanguageModel for WaitingCompaction {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            if !self.ready.load(Ordering::Acquire) {
                return Err(RuntimeModelAuthorityError::AwaitingCredential);
            }
            Arc::clone(&self.inner)
                .revalidate_runtime_authority(checked_at)
                .await
        })
    }

    fn wait_for_runtime_authority(
        self: Arc<Self>,
    ) -> plakous_model::RuntimeModelAuthorityWaitOperation {
        Box::pin(async move {
            self.release.notified().await;
            self.ready.store(true, Ordering::Release);
            Ok(())
        })
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let summary = plan.request().input().items().iter().any(|item| item.blocks().iter().any(|block|
            matches!(block, ContentBlock::Text(text) if text.text() == plakous_model::SEMANTIC_COMPACTION_INSTRUCTION)));
        let prepared = Arc::clone(&self.inner).prepare_call(plan, context)?;
        if summary && self.park_after_prepare && !self.prepared.swap(true, Ordering::AcqRel) {
            self.ready.store(false, Ordering::Release);
        }
        Ok(prepared)
    }
}

#[tokio::test]
async fn idle_compaction_credential_wait_resumes_or_observes_operator_interrupt() -> TestResult {
    for (interrupt, after_prepare) in [(false, false), (true, false), (false, true)] {
        idle_compaction_wait_case(interrupt, after_prepare, false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn idle_compaction_refuses_head_advanced_during_credential_wait() -> TestResult {
    idle_compaction_wait_case(false, false, true).await
}

async fn idle_compaction_wait_case(
    interrupt: bool,
    after_prepare: bool,
    advance_head: bool,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:workflow-login-wait",
        93_000,
    )
    .await?;
    let (mut builder, model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let waiting = Arc::new(WaitingCompaction {
        inner: Arc::clone(&model),
        ready: AtomicBool::new(after_prepare),
        release: Notify::new(),
        park_after_prepare: after_prepare,
        prepared: AtomicBool::new(false),
    });
    builder.model = waiting.clone();
    let interrupted = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&interrupted);
    agent.set_storage_interrupt(Arc::new(move || probe.load(Ordering::Acquire)));
    let endpoint = agent.session_endpoint();
    let compact = tokio::spawn(async move {
        endpoint
            .compact_session(AgentCompactSessionRequest::new(
                "compact:workflow-login-wait".to_owned(),
                source.session_id,
                source.branch_id,
                source.checkpoint_id,
                AgentCompactionReason::Manual,
                builder,
            ))
            .await
    });
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(Duration::from_secs(30), async {
        while agent.credential_wait() != Some(crate::runtime::CredentialWait::SessionWorkflow) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    assert_eq!(waiting.prepared.load(Ordering::Acquire), after_prepare);
    let (other_builder, _) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        agent
            .session_endpoint()
            .compact_session(AgentCompactSessionRequest::new(
                "compact:second-while-parked".to_owned(),
                source.session_id,
                source.branch_id,
                source.checkpoint_id,
                AgentCompactionReason::Manual,
                other_builder,
            )),
    )
    .await?;
    assert!(
        matches!(second, Err(AgentSessionEndpointError::Busy)),
        "{second:?}"
    );
    assert!(
        agent
            .prompt("do not admit a root during parked compaction")
            .await
            .is_err()
    );
    if advance_head {
        let before = agent.checkpoint_id();
        let rejection = agent
            .follow_up(
                TurnId::try_from(uuid::Uuid::from_u128(0x93_ffff))?,
                "this follow-up has no active target",
            )
            .await
            .err()
            .ok_or("follow-up should be refused")?;
        assert_eq!(rejection.kind(), ControlErrorKind::WrongPhase);
        assert_ne!(
            agent.checkpoint_id(),
            before,
            "recorded rejection advances the parked head"
        );
    }
    if interrupt {
        interrupted.store(true, Ordering::Release);
    } else {
        waiting.release.notify_one();
    }
    let result = tokio::time::timeout(Duration::from_secs(30), compact).await??;
    assert!(agent.credential_wait().is_none());
    if advance_head {
        assert!(
            matches!(result, Err(AgentSessionEndpointError::Stale { checkpoint_id, .. })
                if checkpoint_id == agent.checkpoint_id()),
            "{result:?}"
        );
        assert_eq!(model.starts.load(Ordering::Acquire), 0);
    } else if interrupt {
        assert!(!matches!(
            result,
            Ok(AgentCompactSessionOutcome::Compacted { .. })
        ));
        assert_eq!(model.starts.load(Ordering::Acquire), 0);
    } else {
        assert!(
            matches!(result, Ok(AgentCompactSessionOutcome::Compacted { .. })),
            "{result:?}"
        );
        assert_eq!(model.starts.load(Ordering::Acquire), 1);
    }
    agent.shutdown().await?;
    Ok(())
}
