use super::*;

#[tokio::test]
async fn mutating_tool_uses_one_durable_operation_from_intent_through_finalization() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_250,
            false,
        )
        .with_tool_first(),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model;
    let registry = one_mutating_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    let semantic_store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:one-effect-operation", builder)
        .await?;
    let result = agent.prompt("mutate with probe").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected mutating outcome: {:?}",
        result.outcome()
    );
    let effects = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Effect { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        effects.len(),
        1,
        "effect must not create a peer journal operation"
    );
    assert!(
        effects[0]
            .transitions()
            .iter()
            .any(|transition| matches!(transition.state(), DurableOperationState::Claimed(_))),
        "effect transitions omitted the durable claim: {:?}",
        effects[0].transitions()
    );
    assert!(matches!(
        effects[0].state(),
        DurableOperationState::Finalized(_)
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_session_active_is_working_then_terminal_cleanup_finalizes_and_quiesces()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_600,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:process-session-cleanup",
            session_update_builder(workspace, model, registry)?,
        )
        .await?;
    let result = agent.prompt("start process").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected process-session cleanup outcome: {:?}",
        result.outcome()
    );
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let lifetimes = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
        .collect::<Vec<_>>();
    assert_eq!(lifetimes.len(), 1);
    assert!(matches!(
        lifetimes[0].state(),
        DurableOperationState::Finalized(_)
    ));
    assert!(
        lock_mutex(&tool.sessions)
            .as_ref()
            .is_some_and(plakous_tools::JoinedProcessSessions::is_drained)
    );
    let (effect_report_revision, active_sideband_revision) = {
        let stored = lock_mutex(&store.state);
        let effect_report_revision = stored
            .stored_commits
            .values()
            .filter_map(|stored| {
                let manifest = stored.checkpoint().manifest();
                manifest
                    .ledger()
                    .operations()
                    .iter()
                    .any(|record| {
                        matches!(record.kind(), DurableOperationKind::Effect { .. })
                            && matches!(
                                record.state(),
                                DurableOperationState::Completed(_)
                                    | DurableOperationState::ProvenNotDispatched(_)
                            )
                    })
                    .then_some(manifest.topology().branch_revision)
            })
            .min()
            .ok_or("definite effect report never became durable")?;
        let active_sideband_revision = stored
            .stored_commits
            .values()
            .filter_map(|stored| {
                let manifest = stored.checkpoint().manifest();
                manifest
                    .ledger()
                    .operations()
                    .iter()
                    .any(|record| {
                        matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                            && matches!(record.state(), DurableOperationState::Dispatched)
                    })
                    .then_some(manifest.topology().branch_revision)
            })
            .min()
            .ok_or("active process-session sideband never became durable")?;
        (effect_report_revision, active_sideband_revision)
    };
    assert!(
        effect_report_revision < active_sideband_revision,
        "fallible process-session sideband preceded the definite effect report"
    );
    agent.shutdown().await?;
    Ok(())
}

/// The model issues more sequential commands than the backend's concurrent
/// child cap. Every child has already exited before the next tool call, while
/// its result remains available for a later poll. The full driver must settle
/// all durable process lifetimes and finish the turn without inventing an
/// unknown outcome when the backend reclaims old exited handles.
#[cfg(unix)]
#[tokio::test]
async fn sequential_process_sessions_cross_cap_without_losing_cleanup_proof() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let commands = plakous_tools::MAX_JOINED_PROCESS_SESSIONS * 2 + 8;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_650,
            false,
        )
        .with_scripted_tool_calls((0..commands).map(|index| (index, "probe", "{}".to_owned()))),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::ExitedRetained)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:sequential-process-sessions",
            session_update_builder(workspace, model, registry)?
                .config(AgentConfig::default().with_max_model_rounds(commands + 1)),
        )
        .await?;
    let result = agent
        .prompt("run sequential commands")
        .await?
        .result()
        .await?;
    assert_eq!(tool.dispatches.load(Ordering::Acquire), commands);
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected process-session cleanup outcome: {:?}",
        result.outcome()
    );
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let lifetimes = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
        .collect::<Vec<_>>();
    assert_eq!(lifetimes.len(), commands);
    assert!(
        lifetimes
            .iter()
            .all(|record| matches!(record.state(), DurableOperationState::Finalized(_)))
    );
    let next = agent
        .prompt("continue after cleanup")
        .await?
        .result()
        .await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    agent.shutdown().await?;
    Ok(())
}

/// A child can finish before its output is read. The backend joins its
/// supervisor on the first bounded poll, while the tool still reports Active
/// until the fourth page drains. Those later Active pages must not contradict
/// the already-finalized lifetime for the same child and owner generation.
#[cfg(unix)]
#[tokio::test]
async fn exited_process_with_four_output_pages_remains_correlatable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_675,
            false,
        )
        .with_scripted_tool_calls((0..4).map(|index| (index, "probe", "{}".to_owned()))),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::ExitedPaginated)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:exited-paginated-process-session",
            session_update_builder(workspace, model, registry)?
                .config(AgentConfig::default().with_max_model_rounds(5)),
        )
        .await?;
    let result = agent.prompt("drain four pages").await?.result().await?;
    assert_eq!(tool.dispatches.load(Ordering::Acquire), 4);
    assert_eq!(tool.page_cursor.load(Ordering::Acquire), 64);
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected paginated process outcome: {:?}",
        result.outcome()
    );
    let lifetimes = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
        .collect::<Vec<_>>();
    assert_eq!(lifetimes.len(), 1);
    assert!(matches!(
        lifetimes[0].state(),
        DurableOperationState::Finalized(_)
    ));
    assert!(
        lock_mutex(&tool.sessions)
            .as_ref()
            .is_some_and(plakous_tools::JoinedProcessSessions::is_drained)
    );
    agent.shutdown().await?;
    Ok(())
}

/// One exposed active lifetime exits while many later foreground commands
/// finish inside their own Effects. Those Joined updates must reconcile the
/// older lifetime before the backend evicts its bounded proof tombstone.
#[cfg(unix)]
#[tokio::test]
async fn foreground_joins_preserve_an_earlier_active_lifetime_past_the_cap() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let commands = plakous_tools::MAX_JOINED_PROCESS_SESSIONS * 2 + 8;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_680,
            false,
        )
        .with_scripted_tool_calls((0..commands).map(|index| (index, "probe", "{}".to_owned()))),
    );
    let (registry, tool) =
        session_update_registry(SessionUpdateFixtureMode::ActiveThenJoinedForeground)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:foreground-joins-after-active",
            session_update_builder(workspace, model, registry)?
                .config(AgentConfig::default().with_max_model_rounds(commands + 1)),
        )
        .await?;
    let result = agent
        .prompt("run one active then many foreground commands")
        .await?
        .result()
        .await?;
    assert_eq!(tool.dispatches.load(Ordering::Acquire), commands);
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected cleanup outcome: {:?}",
        result.outcome()
    );
    let lifetimes = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
        .collect::<Vec<_>>();
    assert_eq!(lifetimes.len(), 1);
    assert!(matches!(
        lifetimes[0].state(),
        DurableOperationState::Finalized(_)
    ));
    agent.shutdown().await?;
    Ok(())
}

/// A delayed Active sideband can arrive after the backend has evicted the
/// original child's bounded joined-proof tombstone. The durable Finalized
/// lifetime is still exact joined proof and must not be reopened or fenced.
#[cfg(unix)]
#[tokio::test]
async fn stale_active_after_joined_proof_eviction_keeps_lifetime_finalized() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let foreground = plakous_tools::MAX_JOINED_PROCESS_SESSIONS * 2 + 8;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_690,
            false,
        )
        .with_scripted_tool_calls((0..=foreground).map(|index| (index, "probe", "{}".to_owned()))),
    );
    let (registry, tool) =
        session_update_registry(SessionUpdateFixtureMode::ActiveThenJoinedForegroundStale)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:stale-active-after-proof-eviction",
            session_update_builder(workspace, model, registry)?
                .config(AgentConfig::default().with_max_model_rounds(foreground + 2)),
        )
        .await?;
    let result = agent
        .prompt("drain foreground wave then stale page")
        .await?
        .result()
        .await?;
    assert_eq!(tool.dispatches.load(Ordering::Acquire), foreground + 1);
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected stale Active outcome: {:?}",
        result.outcome()
    );
    let lifetimes = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
        .collect::<Vec<_>>();
    assert_eq!(lifetimes.len(), 1);
    assert!(matches!(
        lifetimes[0].state(),
        DurableOperationState::Finalized(_)
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_session_foreground_join_without_prior_lifetime_is_quiescent() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_700,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::JoinedForeground)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:foreground-process-session",
            session_update_builder(workspace, model, registry)?,
        )
        .await?;
    let result = agent
        .prompt("run foreground process")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert!(
        result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .all(|record| !matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
    );
    assert_eq!(tool.dispatches.load(Ordering::Acquire), 1);
    assert!(
        lock_mutex(&tool.sessions)
            .as_ref()
            .is_some_and(plakous_tools::JoinedProcessSessions::is_drained)
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_session_sideband_is_committed_before_concurrent_boundary_error() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_725,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) =
        session_update_registry(SessionUpdateFixtureMode::ActiveAfterCancellation)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:process-session-boundary-race",
            session_update_builder(workspace, model, registry)?,
        )
        .await?;
    let turn = agent
        .prompt("race process update with cancellation boundary")
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while lock_mutex(&tool.sessions).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(
        turn.control().cancel().await?,
        CancellationStatus::Requested
    );
    let result = tokio::time::timeout(Duration::from_secs(3), turn.result()).await??;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert!(
        result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .any(|record| {
                matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                    && matches!(record.state(), DurableOperationState::Finalized(_))
            })
    );
    let correlated = lock_mutex(&store.state)
        .stored_commits
        .values()
        .any(|stored| {
            stored
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .any(|record| {
                    matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                        && matches!(record.state(), DurableOperationState::Dispatched)
                })
        });
    assert!(
        correlated,
        "boundary race omitted the durable process correlation"
    );
    let effect_report_precedes_sideband =
        lock_mutex(&store.state)
            .stored_commits
            .values()
            .any(|stored| {
                let operations = stored.checkpoint().manifest().ledger().operations();
                operations.iter().any(|record| {
                    matches!(record.kind(), DurableOperationKind::Effect { .. })
                        && matches!(
                            record.state(),
                            DurableOperationState::Completed(_)
                                | DurableOperationState::ProvenNotDispatched(_)
                        )
                }) && operations.iter().any(|record| {
                    matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                        && matches!(record.state(), DurableOperationState::Dispatched)
                })
            });
    assert!(
        effect_report_precedes_sideband,
        "definite effect report was not durable before fallible sideband publication"
    );
    assert_eq!(tool.dispatches.load(Ordering::Acquire), 1);
    assert!(
        lock_mutex(&tool.sessions)
            .as_ref()
            .is_some_and(plakous_tools::JoinedProcessSessions::is_drained)
    );
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn process_session_post_start_pre_sideband_error_drains_and_requires_recovery() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_750,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::PostStartError)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:post-start-pre-sideband-cut",
            session_update_builder(workspace, model, registry)?,
        )
        .await?;
    let result = agent
        .prompt("start then fail before sideband")
        .await?
        .result()
        .await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(
        result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .all(|record| !matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
    );
    assert_eq!(tool.dispatches.load(Ordering::Acquire), 1);
    assert!(
        lock_mutex(&tool.sessions)
            .as_ref()
            .is_some_and(plakous_tools::JoinedProcessSessions::is_drained)
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_session_working_reopen_abandons_and_never_redispatches() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            92_800,
            false,
        )
        .with_tool_first()
        .with_block_call_after(1),
    );
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:active-process-reopen",
            session_update_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?,
        )
        .await?;
    let turn = agent.prompt("start recoverable process cut").await?;
    let checkpoint = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let checkpoint = agent.checkpoint();
            if checkpoint
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .any(|record| {
                    matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                        && matches!(record.state(), DurableOperationState::Dispatched)
                })
                && model.calls.load(Ordering::Acquire) >= 2
            {
                break checkpoint;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
            ..
        }
    ));
    let source = DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    store.expire_active_lease();
    let reopening_model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        92_900,
        false,
    ));
    let (reopening_registry, reopening_tool) =
        session_update_registry(SessionUpdateFixtureMode::Active)?;
    let opened = sessions
        .open(
            "open:active-process-reopen",
            source,
            session_update_builder(workspace, Arc::clone(&reopening_model), reopening_registry)?,
        )
        .await?;
    // A joined process whose teardown was never proven is abandoned, not
    // reconciled: the reopen closes it as a running tool and never signals,
    // replays or redispatches anything.
    let DurableRuntimeOpen::Opened(reopened) = opened else {
        return Err("a cut process session is abandoned by the reopen".into());
    };
    let report = reopened
        .abandoned_turn()
        .ok_or("the reopen reports the abandoned turn")?;
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::RunningTool);
    assert_eq!(report.cause(), crate::AbandonmentCause::ProcessLost);
    assert!(matches!(
        reopened.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert!(
        reopened
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
            .all(|record| matches!(record.state(), DurableOperationState::Finalized(_)))
    );
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    assert_eq!(reopening_tool.dispatches.load(Ordering::Acquire), 0);
    reopened.shutdown().await?;
    let _ = agent.shutdown().await;
    drop(turn);
    Ok(())
}

#[tokio::test]
async fn completed_process_evidence_reopen_preserves_settled_work_and_abandons_only_effect()
-> TestResult {
    use crate::runtime::durable::turn_recovery::{
        abandoned_turn_claim, abandoned_turn_recovery_fold,
    };

    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            93_000,
            false,
        )
        .with_tool_first(),
    );
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:completed-process-cut",
            session_update_builder(Arc::clone(&workspace), model, registry)?,
        )
        .await?;
    let turn = agent.prompt("produce joined process evidence").await?;
    let turn_id = turn.id();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let completed = agent.checkpoint();
    agent.shutdown().await?;

    // Use real joined proof and real provider evidence, cutting their transition
    // histories just before finalization, and the effect before its outcome.
    let records = completed.manifest().ledger().operations();
    let provider_id = records
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider missing")?
        .operation_id();
    let effect_id = records
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Effect { .. }))
        .ok_or("effect missing")?
        .operation_id();
    let process_id = records
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
        .ok_or("process lifetime missing")?
        .operation_id();
    let cut_records = records
        .into_iter()
        .map(|record| {
            let cut = if matches!(
                record.kind(),
                DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id
            ) || record.operation_id() == effect_id
            {
                Some(false)
            } else if record.operation_id() == provider_id || record.operation_id() == process_id {
                Some(true)
            } else {
                None
            };
            let Some(completed_cut) = cut else {
                return Ok(record.clone());
            };
            let end = record
                .transitions()
                .iter()
                .position(|transition| {
                    if completed_cut {
                        matches!(transition.state(), DurableOperationState::Completed(_))
                    } else {
                        matches!(transition.state(), DurableOperationState::Dispatched)
                    }
                })
                .ok_or("cut transition missing")?;
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                record.transitions()[..=end].to_vec(),
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let cut = checkpoint_with_code_cell_records(&completed, turn_id, cut_records.clone())?;
    assert!(matches!(
        cut.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
            ..
        }
    ));
    let obligations = cut.manifest().lifecycle().obligations();
    assert_eq!(obligations.len(), 4);
    for obligation in obligations {
        let record = cut_records
            .iter()
            .find(|record| record.operation_id() == obligation.operation_id())
            .ok_or("obligation record missing")?;
        assert!(matches!(
            (obligation.kind(), obligation.action(), record.state()),
            (
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Dispatched
            ) | (
                RecoveryObligationKind::ProviderAttempt | RecoveryObligationKind::ProcessSession,
                RecoveryAction::FinalizationRequired,
                DurableOperationState::Completed(_)
            ) | (
                RecoveryObligationKind::Effect,
                RecoveryAction::EffectEvidenceRequired,
                DurableOperationState::Dispatched
            )
        ));
    }
    assert_eq!(
        abandoned_turn_claim(&abandoned_turn_recovery_fold(&cut)?),
        crate::AbandonedTurnClaim::RunningTool
    );

    // Completed process evidence is not unfinished work. With the effect
    // settled too, it cannot justify abandoning this root at all.
    let boundary_records = cut_records
        .iter()
        .map(|record| {
            if record.operation_id() == effect_id {
                completed
                    .manifest()
                    .ledger()
                    .operations()
                    .into_iter()
                    .find(|original| original.operation_id() == effect_id)
                    .cloned()
                    .ok_or("settled effect missing")
            } else {
                Ok(record.clone())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let boundary =
        checkpoint_with_code_cell_records(&completed, turn_id, boundary_records.clone())?;
    let boundary_error = abandoned_turn_recovery_fold(&boundary)
        .err()
        .ok_or("completed process invented unfinished work")?;
    assert_eq!(boundary_error.kind(), AgentErrorKind::OutcomeUnknown);
    assert_eq!(
        boundary_error.message(),
        "this checkpoint has no unfinished work for a turn to abandon"
    );
    // If only the provider is unfinished, joined proof must not upgrade its
    // claim to RunningTool merely because a process lifetime is present.
    let model_only_records = boundary_records
        .into_iter()
        .map(|record| {
            if record.operation_id() != provider_id {
                return Ok(record);
            }
            let end = record
                .transitions()
                .iter()
                .position(|transition| {
                    matches!(transition.state(), DurableOperationState::Dispatched)
                })
                .ok_or("provider dispatch missing")?;
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                record.transitions()[..=end].to_vec(),
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let model_only = checkpoint_with_code_cell_records(&completed, turn_id, model_only_records)?;
    assert_eq!(
        abandoned_turn_claim(&abandoned_turn_recovery_fold(&model_only)?),
        crate::AbandonedTurnClaim::ModelRequest
    );

    let prior = cut.manifest();
    let topology = completed.manifest().topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let lease = store
        .acquire(AcquireBranch::new(
            session,
            RunId::try_from(uuid::Uuid::from_u128(0x93001))?,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        ))
        .await?;
    let loaded = store.load_branch(session).await?;
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0x93002))?;
    let branch_revision = topology.branch_revision.saturating_add(1);
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: topology.checkpoint_id,
        branch_revision,
        lifecycle: prior.lifecycle(),
        history_identity: prior.canonical().history().identity(),
        ledger: prior.ledger(),
        context_identity: prior.canonical().context().identity(),
        family_artifact: prior.family_artifact(),
    })?;
    let seeded = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision,
            fork: topology.fork,
            lease_fence: lease.fence().get(),
            commit_identity,
        },
        prior.lifecycle().clone(),
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        prior.ledger().clone(),
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::bundle(&seeded, None)?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;

    let reopening_model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        93_100,
        false,
    ));
    let (registry, reopening_tool) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let source = DurableSessionAddress::new(topology.session_id, topology.branch_id, checkpoint_id);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:completed-process-cut",
            source,
            session_update_builder(
                Arc::clone(&workspace),
                Arc::clone(&reopening_model),
                registry.clone(),
            )?,
        )
        .await?
    else {
        return Err("completed process cut did not reopen".into());
    };
    let recovered = reopened.checkpoint();
    assert_eq!(
        reopened
            .abandoned_turn()
            .ok_or("abandonment missing")?
            .claim(),
        crate::AbandonedTurnClaim::RunningTool
    );
    assert!(matches!(
        recovered.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    for original in cut_records {
        let retained = recovered
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| record.operation_id() == original.operation_id())
            .ok_or("recovered operation missing")?;
        if let DurableOperationState::Completed(evidence) = original.state() {
            assert_eq!(
                retained.state(),
                &DurableOperationState::Finalized(evidence.clone())
            );
            assert_eq!(
                &retained.transitions()[..original.transitions().len()],
                original.transitions()
            );
            assert_eq!(
                retained.transitions().len(),
                original.transitions().len() + 1
            );
        } else if matches!(original.state(), DurableOperationState::Finalized(_)) {
            assert_eq!(retained, &original);
        }
        assert!(matches!(
            retained.state(),
            DurableOperationState::Finalized(_)
        ));
    }
    assert_eq!(
        recovered.manifest().canonical().history().segments().len(),
        seeded.manifest().canonical().history().segments().len() + 1
    );
    reopened.shutdown().await?;
    let final_source =
        DurableSessionAddress::new(topology.session_id, topology.branch_id, recovered.id());
    let DurableRuntimeOpen::Opened(reopened_again) = sessions
        .open(
            "open:completed-process-cut-again",
            final_source,
            session_update_builder(workspace, Arc::clone(&reopening_model), registry)?,
        )
        .await?
    else {
        return Err("quiescent recovery did not reopen".into());
    };
    assert_eq!(reopened_again.checkpoint().id(), recovered.id());
    assert_eq!(
        reopened_again.checkpoint().manifest().ledger(),
        recovered.manifest().ledger()
    );
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    assert_eq!(reopening_tool.dispatches.load(Ordering::Acquire), 0);
    reopened_again.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn terminal_aggregate_survives_missing_observations_and_post_terminal_fails() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let terminal_only = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_300,
            false,
        )
        .with_terminal_only(false),
    );
    let agent =
        artifact_test_builder(Arc::clone(&workspace), Arc::clone(&terminal_only)).build()?;
    let result = agent.prompt("terminal only").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(terminal_only.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;

    let duplicate = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_400,
            false,
        )
        .with_terminal_only(true),
    );
    let agent = artifact_test_builder(workspace, Arc::clone(&duplicate)).build()?;
    let result = agent.prompt("duplicate terminal").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_eq!(duplicate.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_joins_the_single_started_prepared_call_exactly_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let joined = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_500,
            false,
        )
        .with_joined_cancellation(Arc::clone(&joined)),
    );
    let agent = artifact_test_builder(workspace, Arc::clone(&model))
        .build()
        .map_err(|error| error.message().to_owned())?;
    let turn = agent.prompt("cancel one prepared call").await?;
    let control = turn.control();
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let checkpoint = agent.checkpoint();
    let topology = checkpoint.manifest().topology();
    let inspection = tokio::time::timeout(
        Duration::from_millis(100),
        agent.session_endpoint().context_status(
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            None,
        ),
    )
    .await?;
    assert_eq!(inspection, Err(AgentSessionEndpointError::Busy));
    assert_eq!(control.cancel().await?, CancellationStatus::Requested);
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    assert!(joined.load(Ordering::Acquire));
    assert_eq!(model.prepares.load(Ordering::Acquire), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn request_cancel_returns_before_the_turn_settles() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_600,
            false,
        )
        .with_tool_first()
        .with_block_call_after(1),
    );
    let agent = sessions
        .create(
            "create:request-cancel-before-settle",
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default()),
        )
        .await?;
    let turn = agent.prompt("hold on the second model call").await?;
    let control = turn.control();
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(!control.is_cancelled());
    assert!(!control.is_settled());
    let status = control.request_cancel().await?;
    assert_eq!(status, CancellationStatus::Requested);
    assert!(control.is_cancelled());
    // Token-first: the request returns without joining settlement. The
    // drive loop may already have settled (token is visible), so this
    // only forbids the API from blocking until the terminal result.
    let result = tokio::time::timeout(Duration::from_secs(2), turn.result()).await??;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    assert!(control.is_settled());
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn queued_cancel_is_one_atomic_publication_and_duplicate_safe() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let joined = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_600,
            false,
        )
        .with_joined_cancellation(Arc::clone(&joined)),
    );
    let agent = sessions
        .create(
            "create:atomic-queued-cancel-success",
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default().with_prompt_queue_capacity(1)),
        )
        .await?;
    let active = agent
        .start(CommandId::new("atomic-cancel:active")?, "active")
        .await?
        .into_turn()
        .ok_or("active turn was not admitted")?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let queued_root_id = CommandId::new("atomic-cancel:queued")?;
    let queued = agent
        .start(queued_root_id.clone(), "queued")
        .await?
        .into_turn()
        .ok_or("queued turn was not admitted")?;
    let queued_turn_id = queued.id();
    let cancel_id = CommandId::new("atomic-cancel:command")?;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    let cancellation = agent
        .command(TurnCommand::Cancel {
            command_id: cancel_id.clone(),
            turn_id: queued_turn_id,
        })
        .await?;
    assert_eq!(
        cancellation.acknowledgement().disposition(),
        CommandAckDisposition::Accepted,
        "queued cancellation was rejected: {}",
        cancellation
            .acknowledgement()
            .rejection()
            .map_or("<missing>", ControlError::message)
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before + 1,
        "queued cancellation must have one checkpoint publication"
    );
    let duplicate = agent
        .command(TurnCommand::Cancel {
            command_id: cancel_id.clone(),
            turn_id: queued_turn_id,
        })
        .await?;
    assert_eq!(
        duplicate.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    assert_eq!(
        duplicate.acknowledgement().sequence(),
        cancellation.acknowledgement().sequence()
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before + 1,
        "duplicate cancel acknowledgement must not republish"
    );
    let result = queued.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    let checkpoint = result.checkpoint();
    let root = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Command {
                    turn_id: Some(turn_id)
                } if *turn_id == queued_turn_id
            ) && record.transitions().len() > 1
        })
        .ok_or("queued root operation was not retained")?;
    let terminal = match root.state() {
        DurableOperationState::Finalized(evidence) => {
            decode_terminal_result_record(evidence.bytes())
        }
        _ => None,
    }
    .ok_or("queued root omitted canonical cancellation evidence")?;
    assert_eq!(terminal.command_id, queued_root_id.as_str());
    assert_eq!(terminal.turn_id, queued_turn_id);
    assert!(matches!(
        terminal.outcome,
        DurableTerminalOutcome::Cancelled(_)
    ));
    let cancel_operation = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            record.transitions().len() == 1
                && matches!(record.state(), DurableOperationState::Finalized(_))
                && serde_json::from_slice::<DurableCommandEnvelope>(record.request().bytes())
                    .is_ok_and(|envelope| {
                        matches!(envelope.effect, DurableActorCommandEffect::CancelRequested)
                    })
        })
        .ok_or("atomic cancel operation was not finalized")?;
    assert!(matches!(
        cancel_operation.kind(),
        DurableOperationKind::Command {
            turn_id: Some(turn_id)
        } if *turn_id == queued_turn_id
    ));
    let replacement = agent
        .start(CommandId::new("atomic-cancel:replacement")?, "replacement")
        .await?;
    assert!(
        replacement.turn().is_some(),
        "cancelled slot was not reusable"
    );
    agent.shutdown().await?;
    let active_result = active.result().await?;
    assert!(matches!(
        active_result.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert!(joined.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test]
async fn failed_atomic_queued_cancel_preserves_queue_and_reports_uncommitted_results() -> TestResult
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
    let descriptor = durable_test_descriptor("test.route")?;
    let joined = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_700,
            false,
        )
        .with_joined_cancellation(Arc::clone(&joined)),
    );
    let agent = sessions
        .create(
            "create:atomic-queued-cancel-failure",
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default().with_prompt_queue_capacity(1)),
        )
        .await?;
    let active = agent.prompt("active").await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let queued_root_id = CommandId::new("atomic-failure:queued")?;
    let queued_submission = agent.start(queued_root_id.clone(), "queued").await?;
    let queued_turn_id = queued_submission
        .turn()
        .ok_or("queued turn was not admitted")?
        .id();
    let queued_control = queued_submission
        .turn()
        .ok_or("queued turn control was unavailable")?
        .control();
    let queued_result = queued_submission
        .result()
        .cloned()
        .ok_or("queued result observer was unavailable")?;
    let before = agent.checkpoint();
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    store.fail_atomic_queued_cancel();
    let cancellation = agent
        .command(TurnCommand::Cancel {
            command_id: CommandId::new("atomic-failure:cancel")?,
            turn_id: queued_turn_id,
        })
        .await?;
    assert_eq!(
        cancellation.acknowledgement().disposition(),
        CommandAckDisposition::Rejected
    );
    assert_eq!(
        cancellation
            .acknowledgement()
            .rejection()
            .ok_or("failed cancel omitted rejection")?
            .kind(),
        ControlErrorKind::Internal
    );
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before + 1
    );
    assert_eq!(agent.checkpoint().id(), before.id());
    assert!(!queued_control.is_cancelled());
    assert!(agent.turn_control(queued_turn_id).is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), queued_result.clone().result())
            .await
            .is_err(),
        "unconfirmed cancel must not settle the queued result"
    );
    let retained_checkpoint = agent.checkpoint();
    let retained_root = retained_checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Command {
                    turn_id: Some(turn_id)
                } if *turn_id == queued_turn_id
            ) && matches!(record.state(), DurableOperationState::Prepared)
        })
        .ok_or("failed cancel did not retain the exact queued obligation")?;
    assert_eq!(retained_root.transitions().len(), 1);
    assert!(
        agent
            .checkpoint()
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .all(|record| record.command_id() != "atomic-failure:cancel")
    );
    let commits_after_failure = store.commit_calls.load(Ordering::Acquire);
    let blocked = agent
        .command(TurnCommand::Start {
            command_id: CommandId::new("atomic-failure:blocked")?,
            input: "blocked".to_owned().into(),
        })
        .await?;
    assert_eq!(
        blocked.acknowledgement().disposition(),
        CommandAckDisposition::Rejected
    );
    assert_eq!(
        blocked
            .acknowledgement()
            .rejection()
            .ok_or("gated admission omitted rejection")?
            .kind(),
        ControlErrorKind::Shutdown
    );
    assert!(blocked.turn().is_none());
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_after_failure,
        "recovery gate must not publish later command classifications"
    );
    let _ = agent.shutdown().await;
    let active_result = active.result().await?;
    assert!(matches!(
        active_result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(active_result.recovery_operation().is_some());
    let queued_result = queued_result.result().await?;
    assert!(matches!(
        queued_result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(queued_result.recovery_operation().is_some());
    Ok(())
}

#[tokio::test]
async fn terminal_checkpoint_failure_returns_exact_recovery_required_authority() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 9_800)?;
    let agent = sessions
        .create(
            "create:terminal-publication-failure",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("terminal publication fails").await?;
    let shared = Arc::clone(&turn.control.shared);
    let result = turn.result().await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(result.recovery_operation().is_some());
    assert_eq!(shared.published_result_count(), 1);
    let checkpoint = agent.checkpoint();
    assert_eq!(result.checkpoint_id(), checkpoint.id());
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Working { .. }
    ));
    assert!(
        checkpoint
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .any(|record| matches!(record.state(), DurableOperationState::Completed(_)))
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn required_durable_refusal_discards_proposed_tool_and_reopens_for_next_turn() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let mut model = artifact_test_model(0, 98_750)?;
    let fixture = Arc::get_mut(&mut model).ok_or("exclusive fixture")?;
    fixture.empty_refusal = true;
    fixture.response_provenance = true;
    fixture.tool_call_on = Some(0);
    let agent = sessions
        .create(
            "create:empty-refusal",
            durable_artifact_test_builder(workspace.clone(), model.clone()),
        )
        .await?;
    let command = CommandId::new("empty-refusal:root")?;
    let result = agent
        .start(command.clone(), "synthetic refusal request")
        .await?
        .into_turn()
        .ok_or("new turn")?
        .result()
        .await?;
    let TurnResultOutcome::Completed(response) = result.outcome() else {
        return Err("refusal did not complete".into());
    };
    assert_eq!(response.finish().reason(), FinishReason::Refusal);
    assert_eq!(response.output(), []);
    assert_eq!(result.tool_calls(), 0);
    assert_exact_terminal_result(&result, &command)?;
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .item_count(),
        1
    );
    assert!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .all(|item| item.actor() == Actor::User)
    );
    agent.shutdown().await?;
    let reopened_model = provenance_test_model(98_800, false)?;
    let DurableRuntimeOpen::Opened(opened) = sessions
        .open(
            "open:empty-refusal",
            source,
            durable_artifact_test_builder(workspace, reopened_model.clone()),
        )
        .await?
    else {
        return Err("empty refusal should reopen normally".into());
    };
    let duplicate = opened
        .start(command.clone(), "synthetic refusal request")
        .await?;
    assert!(duplicate.turn().is_none());
    let restored = duplicate
        .into_result()
        .ok_or("restored terminal")?
        .result()
        .await?;
    let TurnResultOutcome::Completed(restored_response) = restored.outcome() else {
        return Err("refusal terminal was not restored".into());
    };
    assert_eq!(restored_response, response);
    assert_eq!(restored.turn_id(), result.turn_id());
    assert_exact_terminal_result(&restored, &command)?;
    assert_eq!(reopened_model.calls.load(Ordering::Acquire), 0);
    assert_eq!(
        opened.checkpoint().manifest().canonical().history(),
        result.checkpoint().manifest().canonical().history()
    );
    let next = opened
        .prompt("new explicit request")
        .await?
        .result()
        .await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(reopened_model.calls.load(Ordering::Acquire), 1);
    assert!(
        lock_mutex(&reopened_model.observed_provenance)
            .iter()
            .all(|(_, context)| context.previous_response_metadata().is_none())
    );
    assert!(
        next.checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .all(|block| {
                !matches!(
                    block,
                    ContentBlock::ToolCall(_) | ContentBlock::ToolResult(_)
                )
            })
    );
    opened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn terminal_publication_recovery_folds_once_recovers_lost_ack_and_restores_result()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let command_id = CommandId::new("terminal-recovery:root")?;
    let model = artifact_test_model(0, 9_850)?;
    let agent = sessions
        .create(
            "create:terminal-recovery",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model))
                .telemetry(Arc::clone(&telemetry)),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent
        .start(command_id.clone(), "recover exact terminal")
        .await?
        .into_turn()
        .ok_or("new terminal-recovery command was treated as duplicate")?;
    let turn_id = turn.id();
    let recovery_required = turn.result().await?;
    assert!(matches!(
        recovery_required.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(recovery_required.recovery_operation().is_some());
    let checkpoint = agent.checkpoint();
    assert_eq!(recovery_required.checkpoint_id(), checkpoint.id());
    let source = DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::Finalization,
            ..
        }
    ));
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let _ = agent.shutdown().await;
    lock_mutex(&sink.observations).clear();

    let commits_before_recovery = store.commit_calls.load(Ordering::Acquire);
    let invalid = sessions
        .open(
            "open:terminal-recovery-invalid-runtime",
            source,
            durable_artifact_test_builder(Arc::clone(&workspace), artifact_test_model(0, 9_875)?)
                .config(AgentConfig::default().with_prompt_queue_capacity(0)),
        )
        .await
        .expect_err("invalid reconstructed runtime unexpectedly opened");
    assert_eq!(invalid.kind(), AgentErrorKind::InvalidInput);
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before_recovery,
        "failed runtime reconstruction must not publish terminal recovery"
    );
    store.lose_commit_ack_on(commits_before_recovery.saturating_add(1));
    let reopening_model = artifact_test_model(0, 9_900)?;
    let opened = sessions
        .open(
            "open:terminal-recovery",
            source,
            durable_artifact_test_builder(workspace, Arc::clone(&reopening_model))
                .telemetry(telemetry),
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("checkpoint-contained terminal evidence was not recovered".into());
    };
    {
        let observations = lock_mutex(&sink.observations);
        assert!(
            observations.iter().any(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::DurabilityAttemptFinished { .. }
                )
            }),
            "local terminal recovery missing DurabilityAttemptFinished"
        );
        assert!(
            observations.iter().any(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::DurabilityFinished {
                        status: TelemetryTerminalStatus::Completed,
                        ..
                    }
                )
            }),
            "local terminal recovery missing DurabilityFinished"
        );
    }
    assert!(matches!(
        opened.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(
        opened
            .checkpoint()
            .manifest()
            .topology()
            .parent_checkpoint_id,
        Some(source.checkpoint_id())
    );
    assert_eq!(store.lost_commit_acks.load(Ordering::Acquire), 1);
    assert!(store.recover_commit_calls.load(Ordering::Acquire) >= 1);
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);

    for _ in 0..2 {
        let duplicate = opened
            .start(command_id.clone(), "recover exact terminal")
            .await?;
        assert!(duplicate.turn().is_none());
        let restored = duplicate
            .into_result()
            .ok_or("duplicate omitted restored terminal authority")?
            .result()
            .await?;
        assert!(matches!(
            restored.outcome(),
            TurnResultOutcome::Completed(_)
        ));
        assert_eq!(restored.turn_id(), turn_id);
        assert_eq!(restored.checkpoint_id(), opened.checkpoint().id());
    }
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before_recovery.saturating_add(1),
        "terminal restoration or duplicate lookup published more than one successor"
    );
    opened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn terminal_recovery_matrix_promotes_native_artifact_from_receipt_and_zero_dispatch()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let producing_model = artifact_test_model(1, 9_950)?;
    let agent = sessions
        .create(
            "create:receipt-terminal-artifact",
            durable_artifact_test_builder(Arc::clone(&workspace), producing_model),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("native artifact terminal").await?;
    let turn_id = turn.id();
    let recovery_required = turn.result().await?;
    let command_id = CommandId::new(
        agent
            .checkpoint()
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .find(|command| command.turn_id() == Some(turn_id))
            .ok_or("root command")?
            .command_id(),
    )?;
    assert!(matches!(
        recovery_required.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(recovery_required.recovery_operation().is_some());
    let checkpoint = agent.checkpoint();
    assert_eq!(recovery_required.checkpoint_id(), checkpoint.id());
    // The refused consequence commit leaves the recorded decision.
    let response = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find_map(|record| {
            crate::runtime::operation_record::recorded_provider_response(record).transpose()
        })
        .ok_or("recorded terminal decision")??;
    let response = &response;
    let artifact = crate::FamilyArtifact::from_candidate(
        response.family_artifact_candidate().ok_or("candidate")?,
    )?;
    let source = DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    let _ = agent.shutdown().await;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    let reopening_model = artifact_test_model(0, 10_050)?;
    let DurableRuntimeOpen::Opened(opened) = sessions
        .open(
            "open:receipt-terminal-artifact",
            source,
            durable_artifact_test_builder(workspace, Arc::clone(&reopening_model)),
        )
        .await?
    else {
        return Err("receipt-based artifact closure refused".into());
    };
    let recovered = opened.checkpoint();
    assert_eq!(recovered.manifest().family_artifact(), Some(&artifact));
    let terminal = crate::runtime::checkpoint_root_terminal_result(&recovered, &command_id)?
        .ok_or("terminal")?;
    assert_eq!(terminal.response(), Some(response));
    assert_eq!(terminal.usage(), recovery_required.usage());
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before + 1
    );
    opened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn active_dependency_panic_without_terminal_commit_reports_error() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_810,
            false,
        )
        .with_panicking_call(),
    );
    let agent = sessions
        .create(
            "create:active-owner-panic",
            durable_artifact_test_builder(workspace, model),
        )
        .await?;
    let command_id = CommandId::new("active-owner-panic:root")?;
    let turn = agent
        .start(command_id.clone(), "panic while active")
        .await?
        .into_turn()
        .ok_or("panicking root was not admitted")?;
    let shared = Arc::clone(&turn.control.shared);
    let result = turn.result().await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(result.recovery_operation().is_some());
    assert_eq!(result.checkpoint_id(), agent.checkpoint().id());
    assert_eq!(shared.published_result_count(), 1);
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn start_owner_loss_commits_exact_failed_checkpoint_before_publication() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let joined = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_815,
            false,
        )
        .with_joined_cancellation(Arc::clone(&joined)),
    );
    let agent = sessions
        .create(
            "create:start-owner-loss",
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default().with_prompt_queue_capacity(1)),
        )
        .await?;
    let active = agent.prompt("hold owner").await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let command_id = CommandId::new("start-owner-loss:queued")?;
    let queued = agent
        .start(command_id.clone(), "lose owner at start")
        .await?
        .into_turn()
        .ok_or("owner-loss root was not admitted")?;
    let queued_shared = Arc::clone(&queued.control.shared);
    queued_shared.begin_preparing()?;
    active.control().cancel().await?;
    let active_result = active.result().await?;
    assert!(matches!(
        active_result.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    let result = queued.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_exact_terminal_result(&result, &command_id)?;
    assert_eq!(queued_shared.published_result_count(), 1);
    queued_shared.assert_terminal_authority_consistent();
    assert!(joined.load(Ordering::Acquire));
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn missing_terminal_binding_reports_error_without_terminal_authority() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let calls = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_820,
            false,
        )
        .with_joined_cancellation(Arc::clone(&calls)),
    );
    let agent = sessions
        .create(
            "create:missing-terminal-binding",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    let turn = agent.prompt("hold active binding").await?;
    let shared = Arc::clone(&turn.control.shared);
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let admitted_operation = agent
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|operation| {
            matches!(
                operation.kind(),
                DurableOperationKind::Command {
                    turn_id: Some(turn_id)
                } if *turn_id == turn.id()
            )
        })
        .ok_or("admitted root operation missing before fault")?
        .operation_id();
    shared.inject_binding_lookup_fault();
    let _ = agent.shutdown().await;
    let result = turn.result().await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(result.recovery_operation().is_some());
    assert_eq!(result.checkpoint_id(), agent.checkpoint().id());
    assert_eq!(shared.published_result_count(), 1);
    assert!(admitted_operation > 0);
    assert!(calls.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test]
async fn shutdown_publication_faults_retry_the_exact_committed_terminal_once() -> TestResult {
    for fault in 1..=3 {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store,
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let descriptor = durable_test_descriptor("test.route")?;
        let model = Arc::new(
            ArtifactModel::new(
                descriptor.clone(),
                artifact_authority(&descriptor, 1)?,
                0,
                9_830 + u128::from(fault),
                false,
            )
            .with_joined_cancellation(Arc::new(AtomicBool::new(false))),
        );
        let agent = sessions
            .create(
                "create:active-publication-fault",
                durable_artifact_test_builder(workspace, Arc::clone(&model)),
            )
            .await?;
        let command_id = CommandId::new(format!("shutdown-publication-fault:{fault}"))?;
        let turn = agent
            .start(
                command_id.clone(),
                format!("hold for publication fault {fault}"),
            )
            .await?
            .into_turn()
            .ok_or("publication-fault root was not admitted")?;
        let shared = Arc::clone(&turn.control.shared);
        tokio::time::timeout(Duration::from_secs(2), async {
            while model.calls.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        shared.inject_terminal_fault(fault);
        let _ = agent.shutdown().await;
        let result = turn.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
        assert_exact_terminal_result(&result, &command_id)?;
        assert_eq!(shared.published_result_count(), 1);
        shared.assert_terminal_authority_consistent();
    }
    Ok(())
}

#[tokio::test]
async fn persistent_queued_terminal_install_failure_still_cleans_up_and_settles_later_roots()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let joined = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_840,
            false,
        )
        .with_joined_cancellation(joined),
    );
    let agent = sessions
        .create(
            "create:queued-drain-panic",
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default().with_prompt_queue_capacity(3)),
        )
        .await?;
    let active_command = CommandId::new("queued-shutdown-fault:active")?;
    let active = agent
        .start(active_command.clone(), "active")
        .await?
        .into_turn()
        .ok_or("active root was not admitted")?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let queued_command = CommandId::new("queued-shutdown-fault:one")?;
    let queued_two_command = CommandId::new("queued-shutdown-fault:two")?;
    let queued_three_command = CommandId::new("queued-shutdown-fault:three")?;
    let queued = agent
        .start(queued_command.clone(), "queued one")
        .await?
        .into_turn()
        .ok_or("first queued root was not admitted")?;
    let queued_two = agent
        .start(queued_two_command.clone(), "queued two")
        .await?
        .into_turn()
        .ok_or("second queued root was not admitted")?;
    let queued_three = agent
        .start(queued_three_command.clone(), "queued three")
        .await?
        .into_turn()
        .ok_or("third queued root was not admitted")?;
    queued.control.shared.inject_terminal_fault(4);
    let releases = store.releases.load(Ordering::Acquire);
    let flushes = store.flushes.load(Ordering::Acquire);
    assert!(agent.shutdown().await.is_err());
    assert_eq!(store.releases.load(Ordering::Acquire), releases + 1);
    assert_eq!(store.flushes.load(Ordering::Acquire), flushes + 1);
    assert!(queued.result().await.is_err());
    active.result().await?;
    tokio::time::timeout(Duration::from_secs(2), queued_two.result()).await??;
    tokio::time::timeout(Duration::from_secs(2), queued_three.result()).await??;
    Ok(())
}

#[tokio::test]
async fn queued_shutdown_publication_faults_retry_each_exact_checkpoint_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let joined = Arc::new(AtomicBool::new(false));
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_840,
            false,
        )
        .with_joined_cancellation(joined),
    );
    let agent = sessions
        .create(
            "create:queued-drain-panic",
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default().with_prompt_queue_capacity(3)),
        )
        .await?;
    let active_command = CommandId::new("queued-shutdown-fault:active")?;
    let active = agent
        .start(active_command.clone(), "active")
        .await?
        .into_turn()
        .ok_or("active root was not admitted")?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let queued_command = CommandId::new("queued-shutdown-fault:one")?;
    let queued_two_command = CommandId::new("queued-shutdown-fault:two")?;
    let queued_three_command = CommandId::new("queued-shutdown-fault:three")?;
    let queued = agent
        .start(queued_command.clone(), "queued one")
        .await?
        .into_turn()
        .ok_or("first queued root was not admitted")?;
    let queued_two = agent
        .start(queued_two_command.clone(), "queued two")
        .await?
        .into_turn()
        .ok_or("second queued root was not admitted")?;
    let queued_three = agent
        .start(queued_three_command.clone(), "queued three")
        .await?
        .into_turn()
        .ok_or("third queued root was not admitted")?;
    let active_shared = Arc::clone(&active.control.shared);
    let queued_shared = Arc::clone(&queued.control.shared);
    let queued_two_shared = Arc::clone(&queued_two.control.shared);
    let queued_three_shared = Arc::clone(&queued_three.control.shared);
    queued_shared.inject_terminal_fault(1);
    queued_two_shared.inject_terminal_fault(2);
    queued_three_shared.inject_terminal_fault(3);
    let _ = agent.shutdown().await;
    let active_result = active.result().await?;
    let queued_result = queued.result().await?;
    let queued_two_result = queued_two.result().await?;
    let queued_three_result = queued_three.result().await?;
    assert!(matches!(
        active_result.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert!(matches!(
        queued_result.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert!(matches!(
        queued_two_result.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert!(matches!(
        queued_three_result.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert_exact_terminal_result(&active_result, &active_command)?;
    assert_exact_terminal_result(&queued_result, &queued_command)?;
    assert_exact_terminal_result(&queued_two_result, &queued_two_command)?;
    assert_exact_terminal_result(&queued_three_result, &queued_three_command)?;
    assert_eq!(active_shared.published_result_count(), 1);
    assert_eq!(queued_shared.published_result_count(), 1);
    assert_eq!(queued_two_shared.published_result_count(), 1);
    assert_eq!(queued_three_shared.published_result_count(), 1);
    active_shared.assert_terminal_authority_consistent();
    queued_shared.assert_terminal_authority_consistent();
    queued_two_shared.assert_terminal_authority_consistent();
    queued_three_shared.assert_terminal_authority_consistent();
    Ok(())
}
