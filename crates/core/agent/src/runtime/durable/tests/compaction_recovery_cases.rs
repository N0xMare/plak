use super::*;

#[tokio::test]
async fn compaction_recovery_from_completed_checkpoint_is_exact_receipted_and_not_redispatched()
-> TestResult {
    Box::pin(completed_compaction_recovery_case(false)).await
}

#[tokio::test]
async fn large_compaction_request_recovers_the_same_successor_without_redispatch() -> TestResult {
    Box::pin(completed_compaction_recovery_case(true)).await
}

#[tokio::test]
async fn oversized_compaction_request_is_refused_before_paid_dispatch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        workspace.clone(),
        "create:oversized-compaction-input",
        96_000,
    )
    .await?;
    let checkpoint = agent.checkpoint();
    agent.shutdown().await?;
    let identity = DurableCompactionIdentity::new("compact:oversized-input", source)?;
    // Seed the in-memory fixture directly: ordinary request admission must not
    // have to dispatch dozens of rounds to construct an over-limit paid input.
    // Each segment is under the unchanged 16 MiB graph-segment bound.
    let mut segments = Vec::new();
    for index in 0..80_u128 {
        segments.push(HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(97_000 + index * 2))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new(
                    "\0".repeat(150_000),
                    None,
                )?)],
                Provenance::UserInput,
            )?],
        )?);
        segments.push(HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(97_001 + index * 2))?,
                Actor::Assistant,
                vec![ContentBlock::Text(TextBlock::new("closed answer", None)?)],
                Provenance::ModelOutput,
            )?],
        )?);
    }
    segments.push(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(97_200))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "latest protected intent",
                None,
            )?)],
            Provenance::UserInput,
        )?],
    )?);
    let history = CanonicalHistory::new(segments)?;
    assert!(serde_json::to_vec(&history)?.len() > 64 * 1024 * 1024);
    let canonical = RuntimeCanonicalState::new(
        checkpoint.manifest().canonical().stable_prefix().clone(),
        history,
        WorkingProjection::default(),
        checkpoint.manifest().canonical().context().clone(),
    )?;
    let seeded = checkpoint_with_compaction_state(
        &checkpoint,
        identity,
        checkpoint
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect(),
        canonical,
    )?;
    let bundle = RuntimeCheckpointCodec::bundle(&seeded, None)?;
    let reference = CheckpointObjectRef::from_store(seeded.id(), bundle.root(), seeded.integrity());
    let stored = StoredCheckpoint::from_store(reference, bundle)?;
    {
        let mut state = lock_mutex(&store.state);
        let loaded = state
            .loaded
            .as_ref()
            .ok_or("missing oversized input branch")?;
        let head = BranchHead::from_store(
            source.session(),
            BranchRevision::from_store(seeded.manifest().topology().branch_revision),
            reference,
        );
        state.loaded = Some(crate::LoadedBranch::from_store(
            loaded.session(),
            head,
            stored.clone(),
        ));
        state.stored = Some(stored.clone());
        state.stored_commits.insert(source.checkpoint_id, stored);
        state.active_lease = None;
        state.released_lease = None;
    }
    let DurableRuntimeOpen::Opened(agent) = sessions
        .open(
            "open:oversized-input",
            source,
            durable_artifact_test_builder(workspace.clone(), artifact_test_model(0, 98_000)?),
        )
        .await?
    else {
        return Err("oversized source did not reopen".into());
    };
    let before = agent.checkpoint();
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let result = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:oversized-input".to_owned(),
            source.session_id,
            source.branch_id,
            before.id(),
            AgentCompactionReason::Manual,
            builder,
        ))
        .await;
    assert!(
        matches!(result, Err(AgentSessionEndpointError::Rejected { ref reason })
            if reason.contains("runtime reconstruction component violates its byte bound")),
        "oversized paid input must be a pre-dispatch refusal: {result:?}"
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
    assert_eq!(agent.checkpoint().as_ref(), before.as_ref());
    assert!(matches!(
        agent.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}

async fn completed_compaction_recovery_case(large: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let prompts = if large {
        let mut prompts = (0..24)
            .map(|index| format!("closed intent {index}: {}", "x".repeat(200_000)))
            .collect::<Vec<_>>();
        prompts.push("latest protected intent".to_owned());
        prompts
    } else {
        vec![
            "older replaceable intent".to_owned(),
            "latest protected intent".to_owned(),
        ]
    };
    let (agent, source) = seeded_compaction_source_with_prompts(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-completed-cut",
        90_000,
        &prompts,
    )
    .await?;
    let operation_id = "compact:completed-cut";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, live_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    store.halt_after_checkpoint(identity.completed_checkpoint);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected exact source")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.completed_checkpoint
    ));
    let completed = {
        let state = lock_mutex(&store.state);
        state
            .stored_commits
            .get(&identity.completed_checkpoint)
            .cloned()
            .ok_or("completed checkpoint was not durably published")?
    };
    let operations = decode_paid_compaction_operations(completed.checkpoint(), identity)?;
    if large {
        let request = completed
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| record.operation_id() == operations.compaction.0)
            .ok_or("large compaction request missing")?;
        assert!(request.request().bytes().len() > 4 * 1024 * 1024);
        assert_eq!(
            serde_json::to_vec(&operations.request)?,
            request.request().bytes()
        );
        // Cold graph reconstruction must preserve the complete paid input,
        // rather than silently truncating it to the former component limit.
        let bundle = RuntimeCheckpointCodec::bundle(completed.checkpoint(), None)?;
        let cold = crate::CheckpointObjectBundle::from_store(
            bundle.checkpoint_id(),
            bundle.checkpoint_integrity(),
            bundle.root(),
            bundle.objects().to_vec(),
        )?;
        assert_eq!(
            cold.validated_checkpoint()?.as_ref(),
            completed.checkpoint().as_ref()
        );
    }
    let response = completed
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| record.operation_id() == operations.provider.0)
        .and_then(|record| match record.state() {
            DurableOperationState::Completed(evidence) => {
                serde_json::from_slice::<ModelResponse>(evidence.bytes()).ok()
            }
            _ => None,
        })
        .ok_or("completed checkpoint omitted exact provider response")?;
    let recovery_request = operations.prepared.recovery_request()?;
    assert_eq!(recovery.start_count(&recovery_request), 1);
    assert_eq!(live_model.starts.load(Ordering::Acquire), 1);
    assert_eq!(live_model.recover_calls.load(Ordering::Acquire), 0);

    let live_fold = build_compaction_success_fold(
        completed.checkpoint().manifest(),
        identity,
        operations.compaction,
        operations.provider,
        &operations.request,
        &operations.prepared,
        &response,
        live_model.clone(),
        &operations.request.provider_configuration,
        &FixedClock::new(Timestamp::new(10, 0)?),
        Duration::from_secs(1),
    )
    .await;
    let Ok(CompactionSuccessFold {
        history: live_history,
        ledger: live_ledger,
        context: live_context,
        ..
    }) = live_fold
    else {
        return Err("live success fold was unavailable".into());
    };
    let restart_state =
        plakous_test_support::ScriptedRecoveryState::from_snapshot(&recovery.snapshot()?)?;
    let (restart_builder, restart_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        restart_state.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let restart_fold = build_compaction_success_fold(
        completed.checkpoint().manifest(),
        identity,
        operations.compaction,
        operations.provider,
        &operations.request,
        &operations.prepared,
        &response,
        restart_model.clone(),
        &operations.request.provider_configuration,
        &FixedClock::new(Timestamp::new(10, 0)?),
        Duration::from_secs(1),
    )
    .await;
    let Ok(CompactionSuccessFold {
        history: restart_history,
        ledger: restart_ledger,
        context: restart_context,
        ..
    }) = restart_fold
    else {
        return Err("restart success fold was unavailable".into());
    };
    assert_eq!(
        serde_json::to_vec(&live_history)?,
        serde_json::to_vec(&restart_history)?,
        "live and restart history bytes must be identical",
    );
    let live_ledger_bytes = live_ledger
        .operations()
        .into_iter()
        .map(crate::manifest::encode_bundle_operation_record)
        .collect::<Result<Vec<_>, _>>()?;
    let restart_ledger_bytes = restart_ledger
        .operations()
        .into_iter()
        .map(crate::manifest::encode_bundle_operation_record)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        live_ledger_bytes, restart_ledger_bytes,
        "live and restart ledger/evidence bytes must be identical",
    );
    assert_eq!(
        live_context.encode()?,
        restart_context.encode()?,
        "live and restart context bytes must be identical",
    );

    agent.shutdown().await?;
    store.resume_commits();
    store.expire_active_lease();
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    let recovered =
        Box::pin(sessions.recover_compaction(operation_id, source, restart_builder)).await?;
    assert_eq!(
        recovered,
        DurableCompactionRecoveryOutcome::Compacted {
            source,
            target: DurableSessionAddress::new(
                source.session_id,
                source.branch_id,
                identity.success_checkpoint,
            ),
        }
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before.saturating_add(1),
        "completed phase was published twice during restart",
    );
    assert_eq!(restart_model.recover_calls.load(Ordering::Acquire), 0);
    assert_eq!(restart_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(restart_state.start_count(&recovery_request), 1);
    let success = {
        let state = lock_mutex(&store.state);
        state
            .stored_commits
            .get(&identity.success_checkpoint)
            .cloned()
            .ok_or("success checkpoint was not published")?
    };
    assert_eq!(
        success.checkpoint().manifest().canonical().history(),
        &live_history
    );
    assert_eq!(success.checkpoint().manifest().ledger(), &live_ledger);
    assert_eq!(
        success.checkpoint().manifest().canonical().context(),
        &live_context
    );
    // A recovered successor starts unanchored, like a live one: the driver
    // that next opens it seeds its anchor from this state.
    assert_eq!(
        success
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .occupancy_anchor(),
        None
    );
    assert_eq!(
        success
            .checkpoint()
            .manifest()
            .topology()
            .parent_checkpoint_id,
        Some(identity.completed_checkpoint),
    );

    let commits_before_repeat = store.commit_calls.load(Ordering::Acquire);
    let (repeat_builder, repeat_model) = recovery_compaction_fixture(
        workspace,
        restart_state.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let repeated =
        Box::pin(sessions.recover_compaction(operation_id, source, repeat_builder)).await?;
    assert_eq!(repeated, recovered);
    assert_eq!(
        sessions.abandon_compaction(operation_id, source).await?,
        recovered,
        "quiescent success winner was not returned exactly to abandonment",
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before_repeat,
    );
    assert_eq!(repeat_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(repeat_model.recover_calls.load(Ordering::Acquire), 0);
    assert_eq!(restart_state.start_count(&recovery_request), 1);
    Ok(())
}

#[tokio::test]
async fn scheduled_pending_compaction_refuses_unavailable_qualification_before_dispatch()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:scheduled-pending-qualification",
        90_250,
    )
    .await?;
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        Some(1),
    )?;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::with_pending_user_input(
            "compact:scheduled-pending-qualification".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Scheduled,
            Some("private pending sentinel".to_owned().into()),
            builder,
        ))
        .await;
    assert!(matches!(
        outcome,
        Err(AgentSessionEndpointError::Rejected { .. })
    ));
    assert_eq!(model.prepares.load(Ordering::Acquire), 1);
    {
        let prepared = lock_mutex(&model.prepared_requests);
        assert_eq!(prepared.len(), 1);
        assert!(
            prepared[0]
                .windows(b"private pending sentinel".len())
                .any(|window| window == b"private pending sentinel"),
            "scheduled compaction measured the immutable source before its pending qualification"
        );
    }
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before,
        "qualification failure published durable compaction state"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A scheduled compaction on the `OpenAI` Responses route returned its own
/// deadline, and the
/// session was fenced — `compact.finished outcome=recovery_required`, then
/// "continuation is blocked by authoritative recovery-required state; use
/// /clear". The provider had classified the deadline precisely; the compaction
/// wrapper then forced every provider failure to an unknown outcome.
///
/// Compaction publishes nothing until it holds a complete result, so a definite
/// provider failure leaves the canonical history exactly as it was. That is a
/// failed compaction the turn carries on without, not a fence.
///
/// This covers every route that reaches the wrapper, the `OpenAI` Responses and
/// xAI routes included, because the wrapper is the one place the disposition
/// was being overwritten.
#[tokio::test]
async fn a_definite_provider_failure_fails_the_compaction_without_fencing_the_session() -> TestResult
{
    for (kind, message) in [
        (
            ModelErrorKind::Timeout,
            "Responses operation deadline elapsed; no authoritative completion received; \
             observed_phase=Unknown observed_events=4 elapsed_ms=33895",
        ),
        (
            ModelErrorKind::RateLimited,
            "provider rate limited the compaction",
        ),
        (ModelErrorKind::Unavailable, "provider was overloaded"),
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let semantic_store: Arc<dyn SessionStore> = store.clone();
        let sessions = DurableSessionRuntime::current(
            semantic_store,
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?
        .with_store_timeouts(Duration::from_millis(100), Duration::from_millis(25))?;
        let (agent, source) = seeded_compaction_source(
            &sessions,
            Arc::clone(&workspace),
            "create:definite-provider-failure",
            90_500,
        )
        .await?;
        let source_checkpoint = store.load_checkpoint(source.checkpoint_id).await?;
        let operation_id = "compact:definite-provider-failure";
        let identity = DurableCompactionIdentity::new(operation_id, source)?;
        let (builder, model) = recovery_compaction_fixture(
            Arc::clone(&workspace),
            plakous_test_support::ScriptedRecoveryState::new(),
            CompactionFixtureOutcome::AmbiguousFailure(ModelError::new(kind, message)),
            None,
        )?;
        let outcome = agent
            .session_endpoint()
            .compact_session(AgentCompactSessionRequest::new(
                operation_id.to_owned(),
                source.session_id,
                source.branch_id,
                source.checkpoint_id,
                AgentCompactionReason::Manual,
                builder,
            ))
            .await
            .map_err(|_| "compaction endpoint rejected its exact source")?;

        assert!(
            matches!(
                outcome,
                AgentCompactSessionOutcome::Failed { usage: None, .. }
            ),
            "{kind:?} fenced the session instead of failing the compaction: {outcome:?}"
        );
        assert_eq!(model.starts.load(Ordering::Acquire), 1);
        // The canonical history is exactly what it was: nothing was replaced,
        // so there is nothing to recover and the session continues on it.
        let working = agent.checkpoint();
        assert_eq!(
            working.manifest().canonical().history(),
            source_checkpoint
                .checkpoint()
                .manifest()
                .canonical()
                .history()
        );
        let operations = decode_paid_compaction_operations(&working, identity)?;
        for operation in [operations.compaction, operations.provider] {
            assert!(
                !matches!(
                    working
                        .manifest()
                        .ledger()
                        .operations()
                        .into_iter()
                        .find(|record| record.operation_id() == operation.0)
                        .map(DurableOperationRecord::state),
                    Some(DurableOperationState::OutcomeUnknown(_))
                ),
                "{kind:?} left an operation unresolved"
            );
        }
        agent.shutdown().await?;
    }
    Ok(())
}

/// A compaction start that final revalidation refused sent nothing: the fold
/// fails definitely and unpaid (both operations proven not dispatched), and
/// the session is not fenced.
#[tokio::test]
async fn a_dispatch_rejected_compaction_is_proven_not_dispatched() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:dispatch-rejected-compaction",
        90_700,
    )
    .await?;
    let operation_id = "compact:dispatch-rejected";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let rejection = ModelError::new(
        ModelErrorKind::ContextLimit,
        "local admission refused the compaction request",
    )
    .with_dispatch_rejection(plakous_model::ModelDispatchRejection::new(
        plakous_model::ModelDispatchRejectionClass::Admission,
        plakous_model::ModelDispatchRejectionPhase::Revalidation,
    ))?;
    let (builder, model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::DispatchRejected(rejection),
        None,
    )?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected its exact source")?;
    assert!(
        matches!(
            outcome,
            AgentCompactSessionOutcome::Failed { usage: None, .. }
        ),
        "{outcome:?}"
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    let working = agent.checkpoint();
    let operations = decode_paid_compaction_operations(&working, identity)?;
    for operation in [operations.compaction, operations.provider] {
        let record = working
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| record.operation_id() == operation.0)
            .ok_or("compaction operation missing")?;
        let states = record
            .transitions()
            .iter()
            .map(DurableOperationTransition::state)
            .collect::<Vec<_>>();
        assert!(
            states
                .iter()
                .any(|state| matches!(state, DurableOperationState::ProvenNotDispatched(_)))
                && !states.iter().any(|state| matches!(
                    state,
                    DurableOperationState::Completed(_) | DurableOperationState::OutcomeUnknown(_)
                )),
            "a refused start was recorded as possibly sent"
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn no_locator_paid_compaction_is_single_dispatch_and_explicitly_abandonable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?
    .with_store_timeouts(Duration::from_millis(100), Duration::from_millis(25))?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:no-locator-compaction",
        90_500,
    )
    .await?;
    let source_checkpoint = store.load_checkpoint(source.checkpoint_id).await?;
    let operation_id = "compact:no-locator-ambiguous";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery,
        CompactionFixtureOutcome::AmbiguousFailure(ModelError::new(
            ModelErrorKind::OutcomeUnknown,
            "fixture provider outcome is ambiguous",
        )),
        None,
    )?;
    model.omit_recovery_locator();
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "no-locator compaction endpoint rejected exact source")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.failure_evidence_checkpoint
    ));
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
    let working = agent.checkpoint();
    let operations = decode_paid_compaction_operations(&working, identity)?;
    assert!(operations.prepared.recovery_locator().is_none());
    for operation in [operations.compaction, operations.provider] {
        assert!(matches!(
            working
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == operation.0)
                .map(DurableOperationRecord::state),
            Some(DurableOperationState::OutcomeUnknown(_))
        ));
    }
    assert_eq!(
        working.manifest().canonical().history(),
        source_checkpoint
            .checkpoint()
            .manifest()
            .canonical()
            .history()
    );
    let _ = agent.shutdown().await;
    {
        let mut state = lock_mutex(&store.state);
        let dispatched = state
            .stored_commits
            .get(&identity.dispatched_checkpoint)
            .cloned()
            .ok_or("dispatched checkpoint was not durably published")?;
        let head = state
            .commits
            .get(&identity.dispatched_checkpoint)
            .map(|commit| commit.receipt().head())
            .ok_or("dispatched checkpoint lost its exact receipt")?;
        let session = state
            .loaded
            .as_ref()
            .map(crate::LoadedBranch::session)
            .ok_or("compaction branch disappeared")?;
        state.loaded = Some(crate::LoadedBranch::from_store(
            session,
            head,
            dispatched.clone(),
        ));
        state.stored = Some(dispatched);
        state
            .stored_commits
            .remove(&identity.failure_evidence_checkpoint);
        state.commits.remove(&identity.failure_evidence_checkpoint);
        state.active_lease = None;
        state.released_lease = None;
    }

    let (recovery_builder, recovery_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let unresolved = Box::pin(sessions.recover_compaction(operation_id, source, recovery_builder))
        .await
        .map_err(|error| format!("recovery failed: {}", error.message()))?;
    assert!(matches!(
        unresolved,
        DurableCompactionRecoveryOutcome::RecoveryRequired(address)
            if address.checkpoint_id == identity.failure_evidence_checkpoint
    ));
    assert_eq!(recovery_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(recovery_model.recover_calls.load(Ordering::Acquire), 0);
    let recovered_unknown = store
        .load_checkpoint(identity.failure_evidence_checkpoint)
        .await?;
    let dispatched = store
        .load_checkpoint(identity.dispatched_checkpoint)
        .await?;
    assert_eq!(
        recovered_unknown.bundle().base_root(),
        Some(dispatched.reference().root())
    );
    assert!(
        recovered_unknown.bundle().stats().new_object_count()
            < recovered_unknown.bundle().stats().object_count()
    );
    assert_eq!(
        recovered_unknown.bundle().stats().new_object_count(),
        recovered_unknown.bundle().supplied_objects().len()
    );
    let recovered_operations =
        decode_paid_compaction_operations(recovered_unknown.checkpoint(), identity)?;
    for operation in [
        recovered_operations.compaction,
        recovered_operations.provider,
    ] {
        assert!(matches!(
            recovered_unknown
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == operation.0)
                .map(DurableOperationRecord::state),
            Some(DurableOperationState::OutcomeUnknown(_))
        ));
    }
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);

    // The abandonment is one closure commit: there is no intermediate working
    // evidence head to interrupt.
    store.expire_active_lease();
    let abandoned = sessions
        .abandon_compaction(operation_id, source)
        .await
        .map_err(|error| format!("abandonment failed: {}", error.message()))?;
    let target = match abandoned {
        DurableCompactionRecoveryOutcome::Abandoned {
            source: exact,
            target,
        } => {
            assert_eq!(exact, source);
            assert_eq!(
                target.checkpoint_id,
                identity.abandonment_evidence_checkpoint
            );
            target
        }
        other => return Err(format!("unexpected abandonment outcome: {other:?}").into()),
    };
    let target_checkpoint = store.load_checkpoint(target.checkpoint_id).await?;
    assert_eq!(
        target_checkpoint.bundle().base_root(),
        Some(recovered_unknown.reference().root())
    );
    assert!(
        target_checkpoint.bundle().stats().new_object_count()
            < target_checkpoint.bundle().stats().object_count()
    );
    assert_eq!(
        target_checkpoint.bundle().stats().new_object_count(),
        target_checkpoint.bundle().supplied_objects().len()
    );
    assert!(matches!(
        target_checkpoint.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(
        target_checkpoint
            .checkpoint()
            .manifest()
            .canonical()
            .history(),
        source_checkpoint
            .checkpoint()
            .manifest()
            .canonical()
            .history()
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
    assert!(matches!(
        sessions.compaction_receipt(operation_id, source).await?,
        DurableCompactionReceipt::Abandoned {
            source: exact_source,
            target: exact_target,
        } if exact_source == source && exact_target == target
    ));
    let commits_before_repeat = store.commit_calls.load(Ordering::Acquire);
    assert_eq!(
        sessions.abandon_compaction(operation_id, source).await?,
        abandoned,
        "receipt-first repeated abandonment changed its terminal outcome",
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before_repeat,
        "receipt-first repeated abandonment attempted another publication",
    );
    let retry_operation = "compact:no-locator-retry";
    assert_ne!(
        DurableCompactionIdentity::new(retry_operation, target)?.workflow_id,
        identity.workflow_id,
        "an explicit later retry must bind a new operation identity",
    );
    let open_descriptor = durable_test_descriptor("test.route")?;
    let open_authority = artifact_authority(&open_descriptor, 1)?;
    let open_model = RecoveryCompactionModel::new(
        open_descriptor,
        open_authority.clone(),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let open_builder = recovery_session_builder(Arc::clone(&workspace), open_model, open_authority);
    let DurableRuntimeOpen::Opened(retry_agent) = sessions
        .open("open:no-locator-retry", target, open_builder)
        .await?
    else {
        return Err("abandonment target did not reopen as quiescent".into());
    };
    let (retry_builder, retry_model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::DefiniteFailure(ModelError::new(
            ModelErrorKind::Protocol,
            "distinct paid retry failed definitively",
        )),
        None,
    )?;
    let retry_identity = DurableCompactionIdentity::new(retry_operation, target)?;
    let retried = retry_agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            retry_operation.to_owned(),
            target.session_id,
            target.branch_id,
            target.checkpoint_id,
            AgentCompactionReason::Manual,
            retry_builder,
        ))
        .await
        .map_err(|_| "distinct paid retry rejected its abandonment source")?;
    assert!(matches!(
        retried,
        AgentCompactSessionOutcome::Failed { checkpoint, .. }
            if checkpoint == retry_identity.failure_checkpoint
    ));
    assert_eq!(retry_model.starts.load(Ordering::Acquire), 1);
    assert_eq!(retry_model.recover_calls.load(Ordering::Acquire), 0);
    assert!(matches!(
        sessions.compaction_receipt(retry_operation, target).await?,
        DurableCompactionReceipt::Failed {
            source: exact_source,
            target: exact_target,
        } if exact_source == target
            && exact_target.checkpoint_id == retry_identity.failure_checkpoint
    ));
    assert_eq!(
        sessions.abandon_compaction(retry_operation, target).await?,
        DurableCompactionRecoveryOutcome::Failed {
            source: target,
            target: DurableSessionAddress::new(
                target.session_id,
                target.branch_id,
                retry_identity.failure_checkpoint,
            ),
        },
        "ordinary quiescent failure was mislabeled as abandonment",
    );
    assert!(matches!(
        sessions.compaction_receipt(operation_id, source).await?,
        DurableCompactionReceipt::Abandoned { .. }
    ));
    retry_agent.shutdown().await?;
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    Ok(())
}

/// An ordinary refusal of the prepared locator stops the compaction before
/// its provider dispatch, so both operations record their cause as
/// proven-not-dispatched. When that failure evidence commits and the failure
/// publication after it does not, recovery publishes the failure exactly once
/// without reaching the provider.
#[tokio::test]
async fn refused_locator_failure_evidence_recovers_to_one_failure_publication() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-refused-locator",
        91_000,
    )
    .await?;
    let operation_id = "compact:refused-locator";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, live_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    // The compaction's prepare is the next ordinary commit; its prepared
    // locator, the first fence out of `Prepared`, is refused.
    store.refuse_ordinary_commit_on.store(
        store.ordinary_commits.load(Ordering::Acquire) + 2,
        Ordering::Release,
    );
    store.halt_after_checkpoint(identity.failure_evidence_checkpoint);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected exact source")?;
    assert!(
        matches!(
            outcome,
            AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
                if checkpoint == identity.failure_evidence_checkpoint
        ),
        "{outcome:?}"
    );
    assert_eq!(store.ordinary_refusals.load(Ordering::Acquire), 1);
    assert_eq!(live_model.starts.load(Ordering::Acquire), 0);
    let failure_evidence = {
        let state = lock_mutex(&store.state);
        state
            .stored_commits
            .get(&identity.failure_evidence_checkpoint)
            .cloned()
            .ok_or("failure-evidence checkpoint was not durably published")?
    };
    let operations = decode_paid_compaction_operations(failure_evidence.checkpoint(), identity)?;
    for operation in [operations.compaction, operations.provider] {
        assert!(matches!(
            failure_evidence
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == operation.0)
                .map(DurableOperationRecord::state),
            Some(DurableOperationState::ProvenNotDispatched(_))
        ));
    }

    agent.shutdown().await?;
    store.resume_commits();
    store.expire_active_lease();
    let (restart_builder, restart_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    let recovered =
        Box::pin(sessions.recover_compaction(operation_id, source, restart_builder)).await?;
    assert_eq!(
        recovered,
        DurableCompactionRecoveryOutcome::Failed {
            source,
            target: DurableSessionAddress::new(
                source.session_id,
                source.branch_id,
                identity.failure_checkpoint,
            ),
        }
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before.saturating_add(1),
        "recovery published other than the one failure checkpoint",
    );
    assert_eq!(restart_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(restart_model.recover_calls.load(Ordering::Acquire), 0);

    let commits_before_repeat = store.commit_calls.load(Ordering::Acquire);
    let (repeat_builder, repeat_model) =
        recovery_compaction_fixture(workspace, recovery, CompactionFixtureOutcome::Summary, None)?;
    let repeated =
        Box::pin(sessions.recover_compaction(operation_id, source, repeat_builder)).await?;
    assert_eq!(repeated, recovered);
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before_repeat,
    );
    assert_eq!(repeat_model.starts.load(Ordering::Acquire), 0);
    Ok(())
}

/// A crash immediately after the paid compaction's Prepared/Prepared commit
/// leaves exact negative dispatch evidence to recovery. Both operations must
/// take the legal Prepared -> `ProvenNotDispatched` edge; manufacturing a
/// Completed compaction here would make the recovery checkpoint unpublishable.
#[tokio::test]
async fn prepared_compaction_crash_recovers_as_proven_not_dispatched() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:prepared-compaction-crash",
        91_100,
    )
    .await?;
    let operation_id = "compact:prepared-crash";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, live_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    store.halt_after_checkpoint(identity.prepared_checkpoint);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected exact source")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.prepared_checkpoint
    ));
    assert_eq!(live_model.starts.load(Ordering::Acquire), 0);

    agent.shutdown().await?;
    store.resume_commits();
    store.expire_active_lease();
    let (restart_builder, restart_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery,
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let recovered =
        Box::pin(sessions.recover_compaction(operation_id, source, restart_builder)).await?;
    assert!(matches!(
        recovered,
        DurableCompactionRecoveryOutcome::Failed { target, .. }
            if target.checkpoint_id == identity.failure_checkpoint
    ));
    assert_eq!(restart_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(restart_model.recover_calls.load(Ordering::Acquire), 1);
    let evidence = store
        .load_checkpoint(identity.failure_evidence_checkpoint)
        .await?;
    let operations = decode_paid_compaction_operations(evidence.checkpoint(), identity)?;
    for operation in [operations.compaction, operations.provider] {
        assert!(matches!(
            evidence
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == operation.0)
                .map(DurableOperationRecord::state),
            Some(DurableOperationState::ProvenNotDispatched(_))
        ));
    }
    Ok(())
}

#[tokio::test]
async fn compaction_recovery_from_failure_evidence_skips_duplicate_phase_and_is_exact() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-failure-cut",
        91_000,
    )
    .await?;
    let operation_id = "compact:failure-cut";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, live_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery.clone(),
        CompactionFixtureOutcome::DefiniteFailure(ModelError::new(
            ModelErrorKind::Protocol,
            "definite compaction provider failure",
        )),
        None,
    )?;
    store.halt_after_checkpoint(identity.failure_evidence_checkpoint);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected exact failure source")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.failure_evidence_checkpoint
    ));
    let failure_evidence = {
        let state = lock_mutex(&store.state);
        state
            .stored_commits
            .get(&identity.failure_evidence_checkpoint)
            .cloned()
            .ok_or("failure-evidence checkpoint was not durably published")?
    };
    let operations = decode_paid_compaction_operations(failure_evidence.checkpoint(), identity)?;
    let recovery_request = operations.prepared.recovery_request()?;
    assert_eq!(recovery.start_count(&recovery_request), 1);
    assert_eq!(live_model.starts.load(Ordering::Acquire), 1);
    assert_eq!(live_model.recover_calls.load(Ordering::Acquire), 0);
    for operation in [operations.compaction, operations.provider] {
        assert!(matches!(
            failure_evidence
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == operation.0)
                .map(DurableOperationRecord::state),
            Some(DurableOperationState::Completed(_))
        ));
    }

    agent.shutdown().await?;
    store.resume_commits();
    store.expire_active_lease();
    let restart_state =
        plakous_test_support::ScriptedRecoveryState::from_snapshot(&recovery.snapshot()?)?;
    let (restart_builder, restart_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        restart_state.clone(),
        CompactionFixtureOutcome::DefiniteFailure(ModelError::new(
            ModelErrorKind::Protocol,
            "unused restart failure scenario",
        )),
        None,
    )?;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    let recovered =
        Box::pin(sessions.recover_compaction(operation_id, source, restart_builder)).await?;
    assert_eq!(
        recovered,
        DurableCompactionRecoveryOutcome::Failed {
            source,
            target: DurableSessionAddress::new(
                source.session_id,
                source.branch_id,
                identity.failure_checkpoint,
            ),
        }
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before.saturating_add(1),
        "failure-evidence phase was published twice during restart",
    );
    assert_eq!(restart_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(restart_model.recover_calls.load(Ordering::Acquire), 0);
    assert_eq!(restart_state.start_count(&recovery_request), 1);
    let failure = {
        let state = lock_mutex(&store.state);
        state
            .stored_commits
            .get(&identity.failure_checkpoint)
            .cloned()
            .ok_or("failure checkpoint was not published")?
    };
    assert_eq!(
        failure
            .checkpoint()
            .manifest()
            .topology()
            .parent_checkpoint_id,
        Some(identity.failure_evidence_checkpoint),
    );
    assert!(matches!(
        failure.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));

    let commits_before_repeat = store.commit_calls.load(Ordering::Acquire);
    let (repeat_builder, repeat_model) = recovery_compaction_fixture(
        workspace,
        restart_state.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let repeated =
        Box::pin(sessions.recover_compaction(operation_id, source, repeat_builder)).await?;
    assert_eq!(repeated, recovered);
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before_repeat,
    );
    assert_eq!(repeat_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(repeat_model.recover_calls.load(Ordering::Acquire), 0);
    assert_eq!(restart_state.start_count(&recovery_request), 1);
    Ok(())
}

#[tokio::test]
async fn compaction_final_authority_drift_has_zero_physical_starts_and_no_dispatched_state()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store;
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-final-authority-drift",
        91_500,
    )
    .await?;
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let descriptor = model.scripted.descriptor().clone();
    model.drift_final_dispatch_authority(RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"foreign-account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"foreign-credential"])?,
        1,
        Timestamp::new(10, 0)?,
        None,
    )?);
    let result = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:final-authority-drift".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("compaction authority-drift endpoint failed: {error:?}"))?;
    assert!(
        matches!(
            result,
            AgentCompactSessionOutcome::Failed { .. }
                | AgentCompactSessionOutcome::RecoveryRequired { .. }
        ),
        "unexpected authority-drift outcome: {result:?}"
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    assert!(model.authority_checks.load(Ordering::Acquire) >= 4);
    assert!(
        agent
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter(|record| matches!(
                record.kind(),
                DurableOperationKind::Compaction { .. }
                    | DurableOperationKind::Provider {
                        root: ModelOperationRoot::SessionWorkflow(_),
                        ..
                    }
            ))
            .flat_map(DurableOperationRecord::transitions)
            .all(|transition| !matches!(transition.state(), DurableOperationState::Dispatched))
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn paid_compaction_body_outlives_the_operation_timeout_and_commits() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:slow-paid-body",
        95_000,
    )
    .await?;
    let (mut builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    builder.config = builder
        .config
        .with_model_operation_timeout(Duration::from_millis(10));
    model.slow_body.store(true, Ordering::Release);
    let result = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:slow-paid-body".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("compaction endpoint failed: {error:?}"))?;
    assert!(
        matches!(result, AgentCompactSessionOutcome::Compacted { .. }),
        "{result:?}"
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn post_paid_compaction_failures_preserve_working_vs_semantic_failure_taxonomy() -> TestResult
{
    for (case, outcome, fail_prepare_on, entry_base) in [
        (
            "infrastructure",
            CompactionFixtureOutcome::Summary,
            Some(3),
            92_000,
        ),
        (
            "authority-timeout",
            CompactionFixtureOutcome::Summary,
            None,
            94_000,
        ),
        (
            "semantic",
            CompactionFixtureOutcome::InvalidSummary,
            None,
            93_000,
        ),
        (
            "oversized-response",
            CompactionFixtureOutcome::OversizedResponse,
            None,
            95_000,
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let semantic_store: Arc<dyn SessionStore> = store.clone();
        let sessions = DurableSessionRuntime::current(
            semantic_store,
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let (agent, source) = seeded_compaction_source(
            &sessions,
            Arc::clone(&workspace),
            &format!("create:compaction-taxonomy-{case}"),
            entry_base,
        )
        .await?;
        let operation_id = format!("compact:taxonomy-{case}");
        let identity = DurableCompactionIdentity::new(&operation_id, source)?;
        let recovery = plakous_test_support::ScriptedRecoveryState::new();
        let (mut builder, model) = recovery_compaction_fixture(
            Arc::clone(&workspace),
            recovery.clone(),
            outcome,
            fail_prepare_on,
        )?;
        if case == "authority-timeout" {
            model
                .stall_successor_authority
                .store(true, Ordering::Release);
            builder.config = builder
                .config
                .with_model_operation_timeout(Duration::from_millis(10));
        }
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            agent
                .session_endpoint()
                .compact_session(AgentCompactSessionRequest::new(
                    operation_id.clone(),
                    source.session_id,
                    source.branch_id,
                    source.checkpoint_id,
                    AgentCompactionReason::Manual,
                    builder,
                )),
        )
        .await
        .map_err(|_| "successor authority await exceeded its bound")?
        .map_err(|_| "compaction taxonomy endpoint rejected exact source")?;
        assert_eq!(model.starts.load(Ordering::Acquire), 1);
        let prepared = model
            .scripted
            .captured_records()
            .into_iter()
            .next()
            .ok_or("paid compaction call was not captured")?;
        assert_eq!(recovery.start_count(&prepared.recovery_request()?), 1);
        match case {
            "oversized-response" => {
                assert!(
                    matches!(result, AgentCompactSessionOutcome::RecoveryRequired { .. }),
                    "a paid response that cannot be persisted must fence recovery, not become an ordinary endpoint failure: {result:?}"
                );
                let checkpoint = agent.checkpoint();
                assert!(matches!(
                    checkpoint.manifest().lifecycle(),
                    RuntimeLifecycle::Working { .. }
                ));
                assert_ne!(checkpoint.id(), identity.completed_checkpoint);
                assert_ne!(checkpoint.id(), identity.failure_checkpoint);
                assert_eq!(
                    checkpoint.manifest().canonical().history(),
                    store
                        .load_checkpoint(source.checkpoint_id)
                        .await?
                        .checkpoint()
                        .manifest()
                        .canonical()
                        .history()
                );
                assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
                let unresolved = DurableSessionAddress::new(
                    source.session_id,
                    source.branch_id,
                    checkpoint.id(),
                );
                let ledger = checkpoint.manifest().ledger().clone();
                agent.shutdown().await?;
                store.expire_active_lease();
                let restart_state = plakous_test_support::ScriptedRecoveryState::from_snapshot(
                    &recovery.snapshot()?,
                )?;
                let (restart_builder, restart_model) = recovery_compaction_fixture(
                    Arc::clone(&workspace),
                    restart_state,
                    CompactionFixtureOutcome::OversizedResponse,
                    None,
                )?;
                let commits_before = store.commit_calls.load(Ordering::Acquire);
                let recovered =
                    Box::pin(sessions.recover_compaction(&operation_id, source, restart_builder))
                        .await?;
                assert_eq!(
                    recovered,
                    DurableCompactionRecoveryOutcome::RecoveryRequired(unresolved)
                );
                assert_eq!(restart_model.starts.load(Ordering::Acquire), 0);
                assert_eq!(restart_model.recover_calls.load(Ordering::Acquire), 1);
                assert_eq!(store.commit_calls.load(Ordering::Acquire), commits_before);
                assert_eq!(
                    store
                        .load_checkpoint(unresolved.checkpoint_id)
                        .await?
                        .checkpoint()
                        .manifest()
                        .ledger(),
                    &ledger
                );
                continue;
            }
            "infrastructure" | "authority-timeout" => {
                assert!(matches!(
                    result,
                    AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
                        if checkpoint == identity.completed_checkpoint
                ));
                let checkpoint = agent.checkpoint();
                assert_eq!(checkpoint.id(), identity.completed_checkpoint);
                assert!(matches!(
                    checkpoint.manifest().lifecycle(),
                    RuntimeLifecycle::Working { .. }
                ));
                let operations = decode_paid_compaction_operations(&checkpoint, identity)?;
                for operation in [operations.compaction, operations.provider] {
                    assert!(matches!(
                        checkpoint
                            .manifest()
                            .ledger()
                            .operations()
                            .into_iter()
                            .find(|record| record.operation_id() == operation.0)
                            .map(DurableOperationRecord::state),
                        Some(DurableOperationState::Completed(_))
                    ));
                }
            }
            "semantic" => {
                assert!(matches!(
                    result,
                    AgentCompactSessionOutcome::Failed { checkpoint, .. }
                        if checkpoint == identity.failure_checkpoint
                ));
                let checkpoint = agent.checkpoint();
                assert_eq!(checkpoint.id(), identity.failure_checkpoint);
                assert!(matches!(
                    checkpoint.manifest().lifecycle(),
                    RuntimeLifecycle::Quiescent
                ));
                let source_checkpoint = store.load_checkpoint(source.checkpoint_id).await?;
                assert_eq!(
                    checkpoint.manifest().canonical().history(),
                    source_checkpoint
                        .checkpoint()
                        .manifest()
                        .canonical()
                        .history(),
                    "invalid paid response published a partial summary",
                );
            }
            _ => return Err("unknown compaction taxonomy fixture".into()),
        }
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn failed_paid_compaction_keeps_completed_response_usage_separate_from_turn_usage()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let source_model = artifact_test_model(0, 95_000)?;
    let agent = sessions
        .create(
            "create:failed-compact-usage",
            durable_artifact_test_builder(Arc::clone(&workspace), source_model)
                .telemetry(Arc::new(TelemetryEmitter::enabled(sink.clone()))),
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let identity = DurableCompactionIdentity::new("compact:failed-usage", source)?;
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::EmptySummary,
        None,
    )?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:failed-usage".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected its exact source")?;
    let expected = Usage::new(101, 7, 5, 3, 2).with_cost_micros(Some(666));
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::Failed { usage: Some(usage), .. } if usage == expected
    ));
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    let checkpoint = agent.checkpoint();
    let operations = decode_paid_compaction_operations(&checkpoint, identity)?;
    let provider = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| record.operation_id() == operations.provider.0)
        .ok_or("missing exact compaction provider operation")?;
    let DurableOperationState::Finalized(receipt) = provider.state() else {
        return Err("compaction provider response was not finalized".into());
    };
    let response: ModelResponse = serde_json::from_slice(receipt.bytes())?;
    assert_eq!(response.usage(), Some(expected));
    {
        let observed = lock_mutex(&sink.observations);
        assert!(observed.iter().any(|observation| matches!(
            observation.kind(),
            TelemetryObservationKind::CompactionFinished {
                outcome: CompactionEventOutcome::Failed,
                usage: Some(usage),
                ..
            } if usage == expected
        )));
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn decode_compaction_operations_rejects_every_drift_before_recovery_call() -> TestResult {
    #[derive(Clone, Copy, Debug)]
    enum Mutation {
        DuplicateCompaction,
        DuplicateProvider,
        WrongVersion,
        WrongRoot,
        WrongAttempt,
        WrongRequest,
        WrongLocator,
    }

    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-decode-drift",
        94_000,
    )
    .await?;
    let operation_id = "compact:decode-drift";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        recovery,
        CompactionFixtureOutcome::Summary,
        Some(3),
    )?;
    let result = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction decode fixture rejected exact source")?;
    assert!(matches!(
        result,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.completed_checkpoint
    ));
    let valid = agent.checkpoint();
    let decoded = decode_paid_compaction_operations(&valid, identity)?;
    let base_records = valid
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let cases = [
        Mutation::DuplicateCompaction,
        Mutation::DuplicateProvider,
        Mutation::WrongVersion,
        Mutation::WrongRoot,
        Mutation::WrongAttempt,
        Mutation::WrongRequest,
        Mutation::WrongLocator,
    ];
    for mutation in cases {
        let mut records = base_records.clone();
        match mutation {
            Mutation::DuplicateCompaction | Mutation::DuplicateProvider => {
                let target = if matches!(mutation, Mutation::DuplicateCompaction) {
                    decoded.compaction
                } else {
                    decoded.provider
                };
                let record = records
                    .iter()
                    .find(|record| record.operation_id() == target.0)
                    .cloned()
                    .ok_or("duplicate mutation target disappeared")?;
                let next_id = records
                    .iter()
                    .map(DurableOperationRecord::operation_id)
                    .max()
                    .ok_or("compaction ledger was empty")?
                    .saturating_add(1);
                records.push(DurableOperationRecord::new(
                    next_id,
                    record.kind().clone(),
                    record.request().clone(),
                    record.transitions().to_vec(),
                )?);
            }
            Mutation::WrongVersion
            | Mutation::WrongRoot
            | Mutation::WrongAttempt
            | Mutation::WrongRequest => {
                let mut request = decoded.request.clone();
                if matches!(mutation, Mutation::WrongVersion) {
                    request.version = 1;
                } else {
                    let metadata = request.request.metadata();
                    let workflow_id = if matches!(mutation, Mutation::WrongRoot) {
                        SessionWorkflowId::new([7_u8; 32])?
                    } else {
                        identity.workflow_id
                    };
                    let attempt_id = if matches!(mutation, Mutation::WrongAttempt) {
                        AttemptId::try_from(uuid::Uuid::from_u128(94_001))?
                    } else {
                        identity.attempt_id
                    };
                    let created_at = if matches!(mutation, Mutation::WrongRequest) {
                        Timestamp::new(
                            metadata.created_at().seconds().saturating_add(1),
                            metadata.created_at().nanoseconds(),
                        )?
                    } else {
                        metadata.created_at()
                    };
                    request.request = ModelRequest::from_parts(
                        request.request.stable_prefix().clone(),
                        request.request.input().clone(),
                        RequestMetadata::for_session_workflow(
                            metadata.run_id(),
                            workflow_id,
                            attempt_id,
                            created_at,
                        ),
                    )?;
                }
                let replacement = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&request)?,
                )?;
                let index = records
                    .iter()
                    .position(|record| record.operation_id() == decoded.compaction.0)
                    .ok_or("compaction mutation target disappeared")?;
                let record = &records[index];
                records[index] = DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    replacement,
                    record.transitions().to_vec(),
                )?;
            }
            Mutation::WrongLocator => {
                let mut prepared = serde_json::to_value(&decoded.prepared)?;
                let material = prepared
                    .get_mut("recovery_locator")
                    .and_then(|locator| locator.get_mut("provider_material"))
                    .and_then(serde_json::Value::as_array_mut)
                    .and_then(|bytes| bytes.first_mut())
                    .ok_or("prepared locator material was unavailable")?;
                let changed = material.as_u64().unwrap_or_default() ^ 1;
                *material = serde_json::json!(changed);
                let replacement = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&prepared)?,
                )?;
                let index = records
                    .iter()
                    .position(|record| record.operation_id() == decoded.provider.0)
                    .ok_or("provider mutation target disappeared")?;
                let record = &records[index];
                records[index] = DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    replacement,
                    record.transitions().to_vec(),
                )?;
            }
        }
        let poisoned = checkpoint_with_compaction_records(&valid, identity, records)?;
        assert!(
            decode_paid_compaction_operations(&poisoned, identity).is_err(),
            "decode accepted {mutation:?}",
        );
        assert_eq!(
            model.recover_calls.load(Ordering::Acquire),
            0,
            "{mutation:?} reached recover_call",
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn coherent_v2_summary_input_rejects_recovery_and_abandonment_before_side_effects()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        workspace.clone(),
        "create:coherent-summary-input",
        94_500,
    )
    .await?;
    let source_checkpoint = store.load_checkpoint(source.checkpoint_id).await?;
    let operation = "compact:coherent-summary-input";
    let identity = DurableCompactionIdentity::new(operation, source)?;
    let (builder, live_model) = recovery_compaction_fixture(
        workspace.clone(),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        Some(3),
    )?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "coherent input fixture rejected exact source")?;
    assert!(
        matches!(outcome, AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
        if checkpoint == identity.completed_checkpoint)
    );
    assert_eq!(live_model.starts.load(Ordering::Acquire), 1);
    agent.shutdown().await?;

    for phase in [
        identity.dispatched_checkpoint,
        identity.completed_checkpoint,
    ] {
        let valid = store.load_checkpoint(phase).await?;
        let (builder, model) = recovery_compaction_fixture(
            workspace.clone(),
            plakous_test_support::ScriptedRecoveryState::new(),
            CompactionFixtureOutcome::Summary,
            None,
        )?;
        let poisoned = coherently_malformed_compaction_input(valid.checkpoint(), identity, &model)?;
        assert!(
            !crate::runtime::checkpoint_can_abandon_compaction(&poisoned, operation, source),
            "offline rehearsal must refuse coherent malformed compaction input"
        );
        assert_eq!(
            poisoned.manifest().canonical().history(),
            source_checkpoint
                .checkpoint()
                .manifest()
                .canonical()
                .history()
        );
        let operations = decode_paid_compaction_operations(&poisoned, identity)?;
        // Even an empty provider response must not turn malformed input into
        // a semantic failure that can be published as a quiescent descendant.
        let empty = ModelResponse::new(
            Vec::new(),
            FinishDetails::new(FinishReason::Stop, None),
            None,
            None,
            operations.prepared.projection().plan(),
        );
        let fold = build_compaction_success_fold(
            poisoned.manifest(),
            identity,
            operations.compaction,
            operations.provider,
            &operations.request,
            &operations.prepared,
            &empty,
            model.clone(),
            &operations.request.provider_configuration,
            &FixedClock::new(Timestamp::new(10, 0)?),
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(fold, Err(crate::runtime::compaction::CompactionCompletionError::RecoveryRequired(ref error))
            if error.message().contains("not an exact source-derived reduction"))
        );

        let bundle = RuntimeCheckpointCodec::bundle(&poisoned, None)?;
        let reference =
            CheckpointObjectRef::from_store(poisoned.id(), bundle.root(), poisoned.integrity());
        let stored = StoredCheckpoint::from_store(reference, bundle)?;
        let head = {
            let mut state = lock_mutex(&store.state);
            let loaded = state.loaded.as_ref().ok_or("missing compaction branch")?;
            let head = BranchHead::from_store(
                source.session(),
                BranchRevision::from_store(poisoned.manifest().topology().branch_revision),
                reference,
            );
            state.loaded = Some(crate::LoadedBranch::from_store(
                loaded.session(),
                head,
                stored.clone(),
            ));
            state.stored = Some(stored.clone());
            state.stored_commits.insert(phase, stored);
            state.active_lease = None;
            state.released_lease = None;
            head
        };
        let commits_before = store.commit_calls.load(Ordering::Acquire);
        let error = Box::pin(sessions.recover_compaction(operation, source, builder))
            .await
            .expect_err("coherent malformed input must fail before provider recovery");
        assert!(
            error
                .message()
                .contains("not an exact source-derived reduction"),
            "{}",
            error.message()
        );
        let error = sessions
            .abandon_compaction(operation, source)
            .await
            .expect_err("coherent malformed input must not publish abandonment");
        assert!(
            error
                .message()
                .contains("not an exact source-derived reduction"),
            "{}",
            error.message()
        );
        assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
        assert_eq!(model.starts.load(Ordering::Acquire), 0);
        assert_eq!(store.commit_calls.load(Ordering::Acquire), commits_before);
        assert_eq!(store.load_branch(source.session()).await?.head(), head);
        assert_eq!(
            store
                .load_checkpoint(source.checkpoint_id)
                .await?
                .reference(),
            source_checkpoint.reference()
        );
    }
    Ok(())
}

#[tokio::test]
async fn finalized_compaction_receipt_corruption_fails_closed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-corrupt-receipt",
        95_000,
    )
    .await?;
    let operation_id = "compact:corrupt-receipt";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, model) =
        recovery_compaction_fixture(workspace, recovery, CompactionFixtureOutcome::Summary, None)?;
    let compacted = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction receipt fixture rejected exact source")?;
    let AgentCompactSessionOutcome::Compacted {
        source: compacted_source,
        target: compacted_target,
        summary,
        ..
    } = compacted
    else {
        return Err("the compaction receipt fixture must commit".into());
    };
    assert_eq!(compacted_source, source.checkpoint_id);
    assert_eq!(compacted_target, identity.success_checkpoint);
    // A committed fold reports what it did from the successor's own
    // provenance, so the operator line is not a bare "committed".
    let summary = summary.ok_or("a committed compaction must report its figures")?;
    assert_eq!(summary.rung, crate::control::CompactionRung::SemanticFold);
    assert!(summary.segments > 0);
    let valid = {
        let state = lock_mutex(&store.state);
        state
            .commits
            .get(&identity.success_checkpoint)
            .cloned()
            .ok_or("success commit receipt was not retained")?
    };
    let operation = valid.operation();
    let receipt = valid.receipt();
    let wrong_commit_identity = Digest::sha256(
        DigestDomain::ProviderOperation,
        4,
        &[b"corrupt-compaction-commit-identity"],
    )?;
    let wrong_operation = CommittedCheckpoint::from_store(
        CommitOperation::from_store(
            operation.lease().clone(),
            operation.expected(),
            operation.checkpoint(),
            wrong_commit_identity,
        ),
        receipt,
    );
    let source_stored = store.load_checkpoint(source.checkpoint_id).await?;
    let wrong_previous = BranchHead::from_store(
        receipt.previous().session(),
        receipt.previous().revision(),
        source_stored.reference(),
    );
    let wrong_receipt = CommittedCheckpoint::from_store(
        CommitOperation::from_store(
            operation.lease().clone(),
            operation.expected(),
            operation.checkpoint(),
            operation.checkpoint_commit_identity(),
        ),
        CommitReceipt::from_store(
            receipt.checkpoint(),
            wrong_previous,
            receipt.head(),
            receipt.feed_seq(),
        ),
    );
    for (case, corrupted) in [
        ("operation commit identity", wrong_operation),
        ("receipt previous head", wrong_receipt),
    ] {
        lock_mutex(&store.state)
            .commits
            .insert(identity.success_checkpoint, corrupted);
        let result = sessions.compaction_receipt(operation_id, source).await;
        assert!(result.is_err(), "accepted corrupt {case} tuple");
        assert_eq!(
            model.recover_calls.load(Ordering::Acquire),
            0,
            "corrupt {case} tuple reached provider recovery",
        );
    }
    let success_stored = store.load_checkpoint(identity.success_checkpoint).await?;
    let poisoned =
        coherently_malformed_compaction_input(success_stored.checkpoint(), identity, &model)?;
    let bundle = RuntimeCheckpointCodec::bundle(&poisoned, None)?;
    let reference =
        CheckpointObjectRef::from_store(poisoned.id(), bundle.root(), poisoned.integrity());
    let poisoned_stored = StoredCheckpoint::from_store(reference, bundle)?;
    // Keep the store tuple internally consistent: receipt validation must
    // reject the source-derived input, not an unrelated root/integrity mismatch.
    let poisoned_receipt = CommittedCheckpoint::from_store(
        CommitOperation::from_store(
            operation.lease().clone(),
            operation.expected(),
            reference,
            poisoned.checkpoint_commit_identity(),
        ),
        CommitReceipt::from_store(
            reference,
            receipt.previous(),
            BranchHead::from_store(
                receipt.head().session(),
                receipt.head().revision(),
                reference,
            ),
            receipt.feed_seq(),
        ),
    );
    {
        let mut state = lock_mutex(&store.state);
        state
            .stored_commits
            .insert(identity.success_checkpoint, poisoned_stored);
        state
            .commits
            .insert(identity.success_checkpoint, poisoned_receipt);
    }
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    let error = sessions
        .compaction_receipt(operation_id, source)
        .await
        .expect_err("a coherent forged v2 input cannot certify a compaction receipt");
    assert!(
        error
            .message()
            .contains("not an exact source-derived reduction"),
        "{}",
        error.message()
    );
    assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert_eq!(store.commit_calls.load(Ordering::Acquire), commits_before);
    assert_eq!(
        store
            .load_checkpoint(source.checkpoint_id)
            .await?
            .reference(),
        source_stored.reference()
    );
    {
        let mut state = lock_mutex(&store.state);
        state
            .stored_commits
            .insert(identity.success_checkpoint, success_stored);
        state.commits.insert(identity.success_checkpoint, valid);
    }
    assert!(matches!(
        sessions.compaction_receipt(operation_id, source).await?,
        DurableCompactionReceipt::Compacted { .. }
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn compaction_success_fold_is_byte_identical_live_and_after_restart() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-fold-identity",
        96_000,
    )
    .await?;
    let operation_id = "compact:fold-identity";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, live_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    store.halt_after_checkpoint(identity.completed_checkpoint);
    let result = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction fold fixture rejected exact source")?;
    assert!(matches!(
        result,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.completed_checkpoint
    ));
    let completed = agent.checkpoint();
    let operations = decode_paid_compaction_operations(&completed, identity)?;
    let response = completed
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| record.operation_id() == operations.provider.0)
        .and_then(|record| match record.state() {
            DurableOperationState::Completed(evidence) => {
                serde_json::from_slice::<ModelResponse>(evidence.bytes()).ok()
            }
            _ => None,
        })
        .ok_or("completed fold fixture omitted its provider response")?;
    let restarted_state =
        plakous_test_support::ScriptedRecoveryState::from_snapshot(&recovery.snapshot()?)?;
    let (_, restarted_model) = recovery_compaction_fixture(
        workspace,
        restarted_state,
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let clock = FixedClock::new(Timestamp::new(10, 0)?);
    let live = build_compaction_success_fold(
        completed.manifest(),
        identity,
        operations.compaction,
        operations.provider,
        &operations.request,
        &operations.prepared,
        &response,
        live_model,
        &operations.request.provider_configuration,
        &clock,
        Duration::from_secs(1),
    )
    .await;
    let restart = build_compaction_success_fold(
        completed.manifest(),
        identity,
        operations.compaction,
        operations.provider,
        &operations.request,
        &operations.prepared,
        &response,
        restarted_model,
        &operations.request.provider_configuration,
        &clock,
        Duration::from_secs(1),
    )
    .await;
    let (Ok(live), Ok(restart)) = (live, restart) else {
        return Err("live or restart compaction fold was unavailable".into());
    };
    assert_eq!(
        serde_json::to_vec(&live.history)?,
        serde_json::to_vec(&restart.history)?,
    );
    assert_eq!(live.ledger, restart.ledger);
    assert_eq!(
        live.ledger
            .commands()
            .into_iter()
            .map(crate::manifest::encode_bundle_command_record)
            .collect::<Result<Vec<_>, _>>()?,
        restart
            .ledger
            .commands()
            .into_iter()
            .map(crate::manifest::encode_bundle_command_record)
            .collect::<Result<Vec<_>, _>>()?,
    );
    assert_eq!(
        live.ledger
            .operations()
            .into_iter()
            .map(crate::manifest::encode_bundle_operation_record)
            .collect::<Result<Vec<_>, _>>()?,
        restart
            .ledger
            .operations()
            .into_iter()
            .map(crate::manifest::encode_bundle_operation_record)
            .collect::<Result<Vec<_>, _>>()?,
    );
    assert_eq!(live.context.encode()?, restart.context.encode()?);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completed_paid_compaction_can_be_abandoned_without_redispatch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:compaction-completed-abandon",
        97_000,
    )
    .await?;
    let operation_id = "compact:completed-abandon";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery,
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    store.halt_after_checkpoint(identity.completed_checkpoint);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "completed abandonment fixture rejected exact source")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.completed_checkpoint
    ));
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    store.resume_commits();
    store.expire_active_lease();
    let abandoned = sessions.abandon_compaction(operation_id, source).await?;
    let target = match abandoned {
        DurableCompactionRecoveryOutcome::Failed {
            source: exact,
            target,
        }
        | DurableCompactionRecoveryOutcome::Abandoned {
            source: exact,
            target,
        } if exact == source => target,
        other => {
            return Err(format!("completed compaction was not abandonable: {other:?}").into());
        }
    };
    assert_eq!(target.checkpoint_id, identity.failure_checkpoint);
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
    let target_checkpoint = store.load_checkpoint(target.checkpoint_id).await?;
    assert!(matches!(
        target_checkpoint.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    Ok(())
}

#[test]
fn native_compaction_successor_retains_user_groups_and_appends_compaction()
-> Result<(), Box<dyn std::error::Error>> {
    let user = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(1))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("survey the repo", None)?)],
            Provenance::UserInput,
        )?],
    )?;
    let assistant = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(2))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("working", None)?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let call_id = CallId::try_from(uuid::Uuid::from_u128(21))?;
    let correlation = ToolCorrelation::default();
    let tools = HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(22))?,
                Actor::Assistant,
                vec![ContentBlock::ToolCall(ToolCallBlock::new(
                    call_id,
                    ToolName::new("read")?,
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    correlation.clone(),
                    "{}",
                )?)],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(23))?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("ok", None)?)],
                    correlation,
                )?)],
                Provenance::ToolExecution,
            )?,
        ],
    )?;
    let source = CanonicalHistory::new(vec![user.clone(), assistant, tools])?;
    let summary = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(3))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(CompactionBlock::new(
                "native ciphertext summary",
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let successor =
        native_compaction_successor(&source, summary.clone(), NativeSuccessorShape::WithoutTail)?
            .ok_or("tail-less native successor")?;
    assert_eq!(successor.segments().len(), 2);
    assert!(successor.segments().get(0).is_some_and(|segment| {
        segment.kind() == HistorySegmentKind::RecentIntent && segment.shares_storage_with(&user)
    }));
    assert!(successor.segments().get(1).is_some_and(|segment| {
        segment.kind() == HistorySegmentKind::Compaction && segment.identity() == summary.identity()
    }));
    Ok(())
}

#[test]
fn native_compaction_successor_keeps_newest_users_under_retained_budget()
-> Result<(), Box<dyn std::error::Error>> {
    let older = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(11))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "o".repeat(256_000),
                None,
            )?)],
            Provenance::UserInput,
        )?],
    )?;
    let newer = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(12))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("keep newest", None)?)],
            Provenance::UserInput,
        )?],
    )?;
    let source = CanonicalHistory::new(vec![older.clone(), newer.clone()])?;
    let summary = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(13))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(CompactionBlock::new(
                "native ciphertext summary",
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let successor =
        native_compaction_successor(&source, summary.clone(), NativeSuccessorShape::WithoutTail)?
            .ok_or("tail-less native successor")?;
    assert_eq!(successor.segments().len(), 2);
    assert!(successor.segments().get(0).is_some_and(|segment| {
        segment.kind() == HistorySegmentKind::RecentIntent && segment.shares_storage_with(&newer)
    }));
    assert!(successor.segments().get(1).is_some_and(|segment| {
        segment.kind() == HistorySegmentKind::Compaction && segment.identity() == summary.identity()
    }));
    assert!(
        !successor
            .segments()
            .iter()
            .any(|segment| segment.shares_storage_with(&older))
    );
    Ok(())
}

#[test]
fn native_compaction_successor_retains_two_small_users_under_budget()
-> Result<(), Box<dyn std::error::Error>> {
    let first = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(14))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("first intent", None)?)],
            Provenance::UserInput,
        )?],
    )?;
    let second = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(15))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("second intent", None)?)],
            Provenance::UserInput,
        )?],
    )?;
    let source = CanonicalHistory::new(vec![first.clone(), second.clone()])?;
    let summary = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(16))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(CompactionBlock::new(
                "native ciphertext summary",
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let successor =
        native_compaction_successor(&source, summary.clone(), NativeSuccessorShape::WithoutTail)?
            .ok_or("tail-less native successor")?;
    assert_eq!(successor.segments().len(), 3);
    assert!(successor.segments().get(0).is_some_and(|segment| {
        segment.kind() == HistorySegmentKind::Conversation && segment.shares_storage_with(&first)
    }));
    assert!(successor.segments().get(1).is_some_and(|segment| {
        segment.kind() == HistorySegmentKind::RecentIntent && segment.shares_storage_with(&second)
    }));
    assert!(successor.segments().get(2).is_some_and(|segment| {
        segment.kind() == HistorySegmentKind::Compaction && segment.identity() == summary.identity()
    }));
    Ok(())
}

fn native_text_segment(
    kind: HistorySegmentKind,
    entry: u128,
    actor: Actor,
    text: &str,
) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    let provenance = match actor {
        Actor::Assistant => Provenance::ModelOutput,
        _ => Provenance::UserInput,
    };
    Ok(HistorySegment::new(
        kind,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(entry))?,
            actor,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            provenance,
        )?],
    )?)
}

fn native_tool_exchange(
    base: u128,
    output: &str,
) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    let call_id = CallId::try_from(uuid::Uuid::from_u128(base))?;
    let correlation = ToolCorrelation::default();
    Ok(HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![
            ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(base + 1))?,
                Actor::Assistant,
                vec![ContentBlock::ToolCall(ToolCallBlock::new(
                    call_id,
                    ToolName::new("read")?,
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    correlation.clone(),
                    "{}",
                )?)],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(base + 2))?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new(output, None)?)],
                    correlation,
                )?)],
                Provenance::ToolExecution,
            )?,
        ],
    )?)
}

fn native_summary_segment(entry: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    Ok(HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(entry))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(CompactionBlock::new(
                "native ciphertext summary",
            )?)],
            Provenance::ModelOutput,
        )?],
    )?)
}

#[test]
fn native_compaction_successor_keeps_the_current_round_tail_after_the_item()
-> Result<(), Box<dyn std::error::Error>> {
    let older = native_text_segment(
        HistorySegmentKind::Conversation,
        31,
        Actor::User,
        "first intent",
    )?;
    let intent = native_text_segment(
        HistorySegmentKind::RecentIntent,
        32,
        Actor::User,
        "read every page",
    )?;
    let working = native_text_segment(
        HistorySegmentKind::Conversation,
        33,
        Actor::Assistant,
        "reading",
    )?;
    let consumed = native_tool_exchange(40, "page one")?;
    let current = native_tool_exchange(50, "page two says: check word")?;
    let steer = native_text_segment(
        HistorySegmentKind::Steering,
        34,
        Actor::User,
        "also read page three",
    )?;
    let source = CanonicalHistory::new(vec![
        older.clone(),
        intent.clone(),
        working,
        consumed.clone(),
        current.clone(),
        steer.clone(),
    ])?;
    let summary = native_summary_segment(35)?;

    let with_tail =
        native_compaction_successor(&source, summary.clone(), NativeSuccessorShape::WithTail)?
            .ok_or("the current round has a protected tail")?;
    let kept = with_tail.segments().to_vec();
    assert_eq!(kept.len(), 5);
    assert!(kept[0].shares_storage_with(&older));
    assert!(kept[1].shares_storage_with(&intent));
    assert_eq!(kept[2].identity(), summary.identity());
    // The current round's batch and the steering after it, verbatim and in order.
    assert!(kept[3].shares_storage_with(&current));
    assert!(kept[4].shares_storage_with(&steer));
    assert!(
        !kept
            .iter()
            .any(|segment| segment.shares_storage_with(&consumed))
    );

    // The same item without the tail: steering is a retained user message.
    let without_tail =
        native_compaction_successor(&source, summary.clone(), NativeSuccessorShape::WithoutTail)?
            .ok_or("tail-less native successor")?;
    let kept = without_tail.segments().to_vec();
    assert_eq!(kept.len(), 4);
    assert!(kept[2].shares_storage_with(&steer));
    assert_eq!(kept[3].identity(), summary.identity());
    Ok(())
}

#[test]
fn native_compaction_successor_has_no_tail_without_a_current_round_batch()
-> Result<(), Box<dyn std::error::Error>> {
    let summary = native_summary_segment(65)?;
    let answered = CanonicalHistory::new(vec![
        native_text_segment(HistorySegmentKind::RecentIntent, 61, Actor::User, "one")?,
        native_tool_exchange(70, "done")?,
        native_text_segment(
            HistorySegmentKind::Conversation,
            62,
            Actor::Assistant,
            "answered",
        )?,
        native_text_segment(HistorySegmentKind::RecentIntent, 63, Actor::User, "two")?,
    ])?;
    assert!(
        native_compaction_successor(&answered, summary.clone(), NativeSuccessorShape::WithTail)?
            .is_none(),
        "a batch before the latest intent is not the current round"
    );
    let no_tools = CanonicalHistory::new(vec![native_text_segment(
        HistorySegmentKind::RecentIntent,
        64,
        Actor::User,
        "plain",
    )?])?;
    assert!(
        native_compaction_successor(&no_tools, summary, NativeSuccessorShape::WithTail)?.is_none()
    );
    Ok(())
}

/// A harness note opening with the continuation marker.
fn is_continuation_note(segment: &HistorySegment) -> bool {
    crate::runtime::compaction::continuation::is_covered_harness_note(segment)
        && segment
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| {
                matches!(block, ContentBlock::Text(text)
                if text.text().starts_with(
                    crate::runtime::compaction::continuation::TURN_CONTINUATION_NOTE_MARKER,
                ))
            })
}

fn continuation_note_positions(history: &CanonicalHistory) -> Vec<usize> {
    history
        .segments()
        .iter()
        .enumerate()
        .filter(|(_, segment)| is_continuation_note(segment))
        .map(|(index, _)| index)
        .collect()
}

/// The live turn-fence publication of a native successor: the fold, then the
/// turn continuation note it derives.
fn native_turn_fence_successor(
    source: &CanonicalHistory,
    summary: HistorySegment,
    workflow: u8,
) -> Result<CanonicalHistory, Box<dyn std::error::Error>> {
    let successor = native_compaction_successor(source, summary, NativeSuccessorShape::WithTail)?
        .ok_or("the current round has a protected tail")?;
    Ok(
        crate::runtime::compaction::continuation::with_turn_continuation_note(
            successor,
            SessionWorkflowId::new([workflow; 32])?,
        )?,
    )
}

#[test]
fn native_successors_in_one_turn_each_end_with_a_fresh_note_after_the_tail()
-> Result<(), Box<dyn std::error::Error>> {
    let older = native_text_segment(
        HistorySegmentKind::Conversation,
        81,
        Actor::User,
        "plant the seed",
    )?;
    let intent = native_text_segment(HistorySegmentKind::RecentIntent, 82, Actor::User, "grow it")?;
    let consumed = native_tool_exchange(90, "page one")?;
    let first_batch = native_tool_exchange(100, "page two")?;
    let source = CanonicalHistory::new(vec![
        older.clone(),
        intent.clone(),
        consumed,
        first_batch.clone(),
    ])?;
    let first_item = native_summary_segment(83)?;

    // One native compaction: retained groups, the item, the current batch,
    // then the note.
    let first = native_turn_fence_successor(&source, first_item.clone(), 1)?;
    let kept = first.segments().to_vec();
    assert_eq!(kept.len(), 5);
    assert!(kept[0].shares_storage_with(&older));
    assert!(kept[1].shares_storage_with(&intent));
    assert_eq!(kept[2].identity(), first_item.identity());
    assert!(kept[3].shares_storage_with(&first_batch));
    assert_eq!(continuation_note_positions(&first), vec![4]);
    let first_note = kept[4].clone();

    // The turn runs another round, then compacts natively again.
    let second_batch = native_tool_exchange(110, "page three")?;
    let mut grown = kept;
    grown.push(second_batch.clone());
    let second_item = native_summary_segment(84)?;
    let second =
        native_turn_fence_successor(&CanonicalHistory::new(grown)?, second_item.clone(), 2)?;
    let kept = second.segments().to_vec();
    assert_eq!(kept.len(), 5, "the earlier note is not a retained group");
    assert!(kept[0].shares_storage_with(&older));
    assert!(kept[1].shares_storage_with(&intent));
    assert_eq!(kept[2].identity(), second_item.identity());
    assert!(kept[3].shares_storage_with(&second_batch));
    assert_eq!(continuation_note_positions(&second), vec![4]);
    assert_ne!(
        kept[4].identity(),
        first_note.identity(),
        "the second compaction appends its own note"
    );
    // Same text, so no render moves; only the entry differs.
    let text = |segment: &HistorySegment| -> Vec<String> {
        segment
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text().to_owned()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(text(&kept[4]), text(&first_note));
    Ok(())
}

#[test]
fn a_tail_less_native_successor_drops_the_earlier_note_and_gets_none()
-> Result<(), Box<dyn std::error::Error>> {
    let intent = native_text_segment(
        HistorySegmentKind::RecentIntent,
        121,
        Actor::User,
        "grow it",
    )?;
    let steer = native_text_segment(
        HistorySegmentKind::Steering,
        122,
        Actor::User,
        "also water it",
    )?;
    let first = native_turn_fence_successor(
        &CanonicalHistory::new(vec![intent.clone(), native_tool_exchange(130, "one")?])?,
        native_summary_segment(123)?,
        3,
    )?;
    assert_eq!(continuation_note_positions(&first), vec![3]);
    let mut grown = first.segments().to_vec();
    grown.push(steer.clone());
    grown.push(native_tool_exchange(140, "two")?);
    let summary = native_summary_segment(124)?;
    let without_tail = native_compaction_successor(
        &CanonicalHistory::new(grown)?,
        summary.clone(),
        NativeSuccessorShape::WithoutTail,
    )?
    .ok_or("tail-less native successor")?;
    let kept = without_tail.segments().to_vec();
    // User steering stays a retained message; the harness note does not.
    assert_eq!(kept.len(), 3);
    assert!(kept[0].shares_storage_with(&intent));
    assert!(kept[1].shares_storage_with(&steer));
    assert_eq!(kept[2].identity(), summary.identity());
    let published = crate::runtime::compaction::continuation::with_turn_continuation_note(
        without_tail,
        SessionWorkflowId::new([4; 32])?,
    )?;
    assert_eq!(continuation_note_positions(&published), Vec::<usize>::new());
    Ok(())
}

fn person_steering(entries: &[(u128, &str)]) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    Ok(HistorySegment::new(
        HistorySegmentKind::Steering,
        entries
            .iter()
            .map(|(entry, text)| {
                ConversationItem::new(
                    EntryId::try_from(uuid::Uuid::from_u128(*entry))?,
                    Actor::User,
                    vec![ContentBlock::Text(TextBlock::new(*text, None)?)],
                    Provenance::UserInput,
                )
                .map_err(Into::into)
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?,
    )?)
}

fn harness_note(entry: u128, text: &str) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    Ok(
        crate::runtime::compaction::placeholder::harness_note_segment(
            EntryId::try_from(uuid::Uuid::from_u128(entry))?,
            text.to_owned(),
        )?,
    )
}

#[test]
fn a_native_successor_keeps_person_steering_that_opens_like_the_continuation_note()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::runtime::compaction::continuation::TURN_CONTINUATION_NOTE_MARKER;
    let marked = format!("{TURN_CONTINUATION_NOTE_MARKER} my own words");
    let intent = native_text_segment(
        HistorySegmentKind::RecentIntent,
        201,
        Actor::User,
        "grow it",
    )?;
    let single = person_steering(&[(202, &marked)])?;
    let multi = person_steering(&[(203, &marked), (204, "and one more thing")])?;
    let source = CanonicalHistory::new(vec![
        intent.clone(),
        native_tool_exchange(210, "one")?,
        single.clone(),
        multi.clone(),
        native_tool_exchange(220, "two")?,
    ])?;
    let summary = native_summary_segment(205)?;
    let published = native_turn_fence_successor(&source, summary.clone(), 7)?;
    let kept = published.segments().to_vec();
    assert_eq!(kept.len(), 6);
    assert!(kept[0].shares_storage_with(&intent));
    assert!(
        kept[1].shares_storage_with(&single),
        "single-item steering stays"
    );
    assert!(
        kept[2].shares_storage_with(&multi),
        "multi-item steering stays"
    );
    assert_eq!(kept[3].identity(), summary.identity());
    assert_eq!(kept[4].kind(), HistorySegmentKind::ToolExchange);
    // A person's steering is not the continuation note, so the turn still
    // gets one after its tail.
    assert_eq!(continuation_note_positions(&published), vec![5]);
    Ok(())
}

#[test]
fn a_native_successor_retains_no_harness_note_and_spends_no_budget_on_one()
-> Result<(), Box<dyn std::error::Error>> {
    // 200 000 bytes is 50 000 tokens: it fits the 64 000-token retained
    // budget alone, but not after a 25 000-token harness note.
    let older = native_text_segment(
        HistorySegmentKind::Conversation,
        231,
        Actor::User,
        &"o".repeat(200_000),
    )?;
    let intent = native_text_segment(
        HistorySegmentKind::RecentIntent,
        232,
        Actor::User,
        "grow it",
    )?;
    let plan = harness_note(233, &format!("PLAN\n{}", "p".repeat(100_000)))?;
    let other = harness_note(234, "a placeholder pass removed two results")?;
    let source = CanonicalHistory::new(vec![
        older.clone(),
        intent.clone(),
        native_tool_exchange(240, "one")?,
        plan,
        other,
        native_tool_exchange(250, "two")?,
    ])?;
    let summary = native_summary_segment(235)?;
    for shape in [
        NativeSuccessorShape::WithTail,
        NativeSuccessorShape::WithoutTail,
    ] {
        let successor = native_compaction_successor(&source, summary.clone(), shape)?
            .ok_or("native successor")?;
        let kept = successor.segments().to_vec();
        assert!(kept[0].shares_storage_with(&older), "{shape:?}");
        assert!(kept[1].shares_storage_with(&intent), "{shape:?}");
        assert_eq!(kept[2].identity(), summary.identity(), "{shape:?}");
        assert!(
            !kept
                .iter()
                .any(crate::runtime::compaction::continuation::is_covered_harness_note),
            "{shape:?}"
        );
    }
    Ok(())
}

#[test]
fn a_plan_shown_in_the_tail_leaves_no_plan_note_above_the_item()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::runtime::compaction::continuation::{
        is_covered_harness_note, with_post_compaction_note, with_turn_continuation_note,
    };
    let intent = native_text_segment(
        HistorySegmentKind::RecentIntent,
        261,
        Actor::User,
        "grow it",
    )?;
    let first = native_turn_fence_successor(
        &CanonicalHistory::new(vec![intent.clone(), native_tool_exchange(270, "one")?])?,
        native_summary_segment(262)?,
        8,
    )?;
    let mut grown = first.segments().to_vec();
    // The first publication's plan note, before its continuation note.
    grown.insert(
        grown.len() - 1,
        harness_note(263, "PLAN\n- [pending] old step")?,
    );
    grown.push(native_tool_exchange(
        280,
        "revision=2\n- [completed] old step",
    )?);
    let source = CanonicalHistory::new(grown)?;
    // The newest batch already shows the plan, so the note function derives
    // nothing for this successor.
    let shown: crate::runtime::PostCompactionNote = Arc::new(|_, _| None);
    let summary = native_summary_segment(264)?;
    let successor =
        native_compaction_successor(&source, summary.clone(), NativeSuccessorShape::WithTail)?
            .ok_or("tail")?;
    let published = with_turn_continuation_note(
        with_post_compaction_note(successor, &source, Some(&shown), || {
            EntryId::try_from(uuid::Uuid::from_u128(265))
                .map_err(|error| AgentError::from_identity(&error))
        })?,
        SessionWorkflowId::new([9; 32])?,
    )?;
    let kept = published.segments().to_vec();
    assert_eq!(kept.len(), 4, "intent, item, tail, continuation note");
    assert!(kept[0].shares_storage_with(&intent));
    assert_eq!(kept[1].identity(), summary.identity());
    assert_eq!(kept[2].kind(), HistorySegmentKind::ToolExchange);
    assert!(!kept.iter().take(3).any(is_covered_harness_note));
    assert_eq!(continuation_note_positions(&published), vec![3]);
    Ok(())
}

#[test]
fn a_native_successor_keeps_process_notices_on_first_and_repeated_compactions()
-> Result<(), Box<dyn std::error::Error>> {
    let intent = native_text_segment(
        HistorySegmentKind::RecentIntent,
        291,
        Actor::User,
        "build it",
    )?;
    let notice = harness_note(
        292,
        "[harness]\n<process_notice id=\"p1\" status=\"outcome_unknown\" elapsed=\"0m9s\">\n\
         command: cargo build\nstarted: 2026-10-04T12:00:03Z\nPlak stopped unexpectedly while \
         this command was running. It may still be running outside Plak and may hold locks or \
         ports; the runtime cap no longer applies. Check before relying on its effects.\n\
         </process_notice>",
    )?;
    let plan = harness_note(293, "PLAN\n- [pending] build")?;
    let source = CanonicalHistory::new(vec![
        intent.clone(),
        native_tool_exchange(300, "spawned")?,
        notice.clone(),
        plan,
        native_tool_exchange(310, "one")?,
    ])?;
    let first_item = native_summary_segment(294)?;
    let first = native_turn_fence_successor(&source, first_item.clone(), 10)?;
    let kept = first.segments().to_vec();
    assert_eq!(
        kept.len(),
        5,
        "intent, notice, item, tail, continuation note"
    );
    assert!(kept[0].shares_storage_with(&intent));
    assert!(
        kept[1].shares_storage_with(&notice),
        "the process notice stays"
    );
    assert_eq!(kept[2].identity(), first_item.identity());
    assert_eq!(continuation_note_positions(&first), vec![4]);

    let mut grown = kept;
    grown.push(native_tool_exchange(320, "two")?);
    let second_item = native_summary_segment(295)?;
    let second =
        native_turn_fence_successor(&CanonicalHistory::new(grown)?, second_item.clone(), 11)?;
    let kept = second.segments().to_vec();
    assert_eq!(kept.len(), 5);
    assert!(
        kept[1].shares_storage_with(&notice),
        "and survives the next one"
    );
    assert_eq!(kept[2].identity(), second_item.identity());
    assert_eq!(continuation_note_positions(&second), vec![4]);
    Ok(())
}
