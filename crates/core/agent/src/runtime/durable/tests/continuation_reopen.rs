use super::*;

fn repeated_pause_model(entry_base: u128) -> TestResult<Arc<ArtifactModel>> {
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    Ok(Arc::new(
        ArtifactModel::new(descriptor, authority, 2, entry_base, false)
            .with_provider_pauses([0, 1]),
    ))
}

fn assert_repeated_pause_result(model: &ArtifactModel, result: &TurnResult) {
    if let TurnResultOutcome::Failed(error) = result.outcome() {
        panic!("repeated pause failed: {}", error.message());
    }
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected repeated-pause outcome: {:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 3);
    assert_eq!(model.observed().len(), 3);
    assert!(model.observed()[0].is_none());
    assert!(model.observed()[1].is_some());
    assert!(model.observed()[2].is_some());
    let pauses = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::ServerPauseContinuation { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(pauses.len(), 2);
    assert!(
        pauses
            .iter()
            .all(|record| matches!(record.state(), DurableOperationState::Finalized(_)))
    );
    assert!(pauses.iter().all(|record| {
        let transitions = record.transitions();
        transitions.len() == 2
            && matches!(
                transitions[0].state(),
                DurableOperationState::ContinuationRequired(_)
            )
            && matches!(transitions[1].state(), DurableOperationState::Finalized(_))
    }));
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
}

#[tokio::test]
async fn memory_driver_repeats_provider_server_pause_without_terminalizing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = repeated_pause_model(9_000)?;
    let agent = artifact_test_builder(workspace, Arc::clone(&model)).build()?;
    let result = agent.prompt("repeat server pause").await?.result().await?;
    assert_repeated_pause_result(model.as_ref(), &result);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_driver_repeats_provider_server_pause_without_terminalizing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = repeated_pause_model(9_100)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:repeated-server-pause",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    let result = agent.prompt("repeat server pause").await?.result().await?;
    assert_repeated_pause_result(model.as_ref(), &result);
    let promoted = lock_mutex(&store.state)
        .stored_commits
        .values()
        .filter_map(|stored| stored.bundle().validated_checkpoint().ok())
        .find(|checkpoint| {
            matches!(checkpoint.manifest().lifecycle(), RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                    obligations,
                    ..
                } if obligations.len() == 2)
        })
        .ok_or("durable run omitted its promoted hosted baton")?;
    crate::recovery_scope::require_recoverable(&promoted)?;
    assert!(matches!(
        decode_hosted_continuation(&promoted)?,
        RestoredContinuationFrame::ServerPause(frame) if frame.promoted
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_promoted_server_pause_reopens_and_completes_without_replay() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_150,
            false,
        )
        .with_provider_pauses([0]),
    );
    let store = Arc::new(TestCreateStore::default());
    store.block_after_promoted_hosted();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:promoted-server-pause",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let root = CommandId::new("root:promoted-server-pause")?;
    let submission = original
        .start(root.clone(), "interrupt after promoted server pause")
        .await?;
    let admission_seq = submission
        .acknowledgement()
        .feed_seq()
        .ok_or("admission sequence")?;
    let _turn = submission.into_turn().ok_or("root turn")?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire)
            && matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                    obligations,
                    ..
                } if obligations.len() == 2
            )
        {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open(
            "open:promoted-server-pause",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?
    else {
        return Err("promoted server pause did not restore as working".into());
    };
    let result = turn.result().result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "promoted server-pause reopen failed: {:?}; calls={}; lifecycle={:?}; operations={:?}",
        result.outcome(),
        model.calls.load(Ordering::Acquire),
        result.checkpoint().manifest().lifecycle(),
        result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|record| (record.operation_id(), record.kind(), record.state()))
            .collect::<Vec<_>>()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let duplicate = agent
        .start(root, "interrupt after promoted server pause")
        .await?;
    assert_eq!(
        duplicate.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    assert_eq!(
        duplicate.acknowledgement().feed_seq(),
        Some(admission_seq),
        "working recovery preserves the original admission record"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A parked continuation frame carries the anchor across a process loss: the
/// resumed round reports no usage, so the anchor the turn terminal records is
/// the one restored from the frame.
#[tokio::test]
async fn a_restored_server_pause_resumes_with_the_frame_anchor() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_160,
            false,
        )
        .with_provider_pauses([0])
        .with_usage_on_calls([0]),
    );
    let store = Arc::new(TestCreateStore::default());
    store.block_after_promoted_hosted();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:anchored-server-pause",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let _turn = original.prompt("pause with an anchor").await?;
    let parked = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire)
            && matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                    obligations,
                    ..
                } if obligations.len() == 2
            )
        {
            break checkpoint;
        }
        tokio::task::yield_now().await;
    };
    let restored_frame = decode_hosted_continuation(&parked)?;
    assert_eq!(
        restored_frame.budgets(),
        crate::runtime::operation_record::TurnBudgets {
            model_rounds: 1,
            ..Default::default()
        }
    );
    let RestoredContinuationFrame::ServerPause(frame) = &restored_frame else {
        return Err("missing parked server-pause frame".into());
    };
    let mut spent = frame.frame.clone();
    spent.budgets.overflow_recovery_taken = true;
    spent.budgets.paid_fold_spent = true;
    spent.budgets.pressure_fold_spent = true;
    let spent_budgets = spent.budgets;
    let encoded = serde_json::to_vec(
        &crate::runtime::continuation::RuntimeContinuation::AwaitServerPauseV1(Box::new(spent)),
    )?;
    let crate::runtime::continuation::RuntimeContinuation::AwaitServerPauseV1(decoded) =
        serde_json::from_slice(&encoded)?
    else {
        return Err("server-pause guards changed frame kind".into());
    };
    assert_eq!(decoded.budgets, spent_budgets);
    let frame_anchor = restored_frame
        .context_anchor()
        .ok_or("the parked frame must carry the round's anchor")?;
    assert_eq!(frame_anchor.provider_input_tokens(), 5);
    assert_eq!(
        parked.manifest().canonical().context().occupancy_anchor(),
        Some(frame_anchor),
        "the parked checkpoint publishes the anchor its frame carries"
    );
    let address =
        DurableSessionAddress::new(original.session_id(), original.branch_id(), parked.id());
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open(
            "open:anchored-server-pause",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?
    else {
        return Err("anchored server pause did not restore as working".into());
    };
    let result = turn.result().result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(
        result
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .occupancy_anchor(),
        Some(frame_anchor)
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_promoted_programmatic_tool_batch_reopens_and_completes_without_replay()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_175,
            false,
        )
        .with_tool_first()
        .with_programmatic_local_tool()?,
    );
    let registry = one_mutating_tool_registry()?;
    let store = Arc::new(TestCreateStore::default());
    store.block_after_promoted_hosted();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:promoted-programmatic-tool",
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
        .await?;
    let _turn = original
        .prompt("interrupt after promoted programmatic tool")
        .await?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire)
            && matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                    obligations,
                    ..
                } if obligations.len() == 2
            )
        {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open(
            "open:promoted-programmatic-tool",
            address,
            durable_decision_builder(workspace, Arc::clone(&model), registry)?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
        .await?
    else {
        return Err("promoted programmatic tool batch did not restore as working".into());
    };
    let result = turn.result().result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "promoted tool-batch reopen failed: {:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}

async fn reopen_hosted_tool_batch_after_completed_prefix(
    completed_prefix: usize,
    two_calls: bool,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        1,
        if two_calls { 9_185 } else { 9_180 },
        false,
    )
    .with_tool_first();
    fixture = if two_calls {
        fixture.with_mixed_programmatic_and_direct_tools()?
    } else {
        fixture.with_programmatic_local_tool()?
    };
    let model = Arc::new(fixture);
    let registry = if two_calls {
        mutating_tool_registry_with_outcomes(2)?
    } else {
        one_mutating_tool_registry()?
    };
    let store = Arc::new(TestCreateStore::default());
    store.block_after_hosted_effect_count(completed_prefix);
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            if two_calls {
                "create:hosted-tool-prefix"
            } else {
                "create:hosted-tool-initial"
            },
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
        .await?;
    let _turn = original.prompt("interrupt hosted tool batch").await?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open(
            if two_calls {
                "open:hosted-tool-prefix"
            } else {
                "open:hosted-tool-initial"
            },
            address,
            durable_decision_builder(workspace, Arc::clone(&model), registry)?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
        .await?
    else {
        return Err("hosted tool-batch crash cut did not restore as working".into());
    };
    let result = turn.result().result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "hosted prefix reopen failed: {:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_initial_hosted_tool_batch_reopens_without_provider_replay() -> TestResult {
    Box::pin(reopen_hosted_tool_batch_after_completed_prefix(0, false)).await
}

#[tokio::test]
async fn durable_hosted_tool_prefix_reopens_without_replaying_completed_effect() -> TestResult {
    Box::pin(reopen_hosted_tool_batch_after_completed_prefix(1, true)).await
}

/// A resumed turn above its round cap does not wrap up a second time.
///
/// `run_turn_from_state` is re-entered by every continuation resume with the
/// round count restored but this frame's locals lost. Initialising the wrap-up
/// flag to `false` would give a crash during a paused wrap-up round a second
/// note and a second tool-free round, which spec 01 §3.3 forbids: the budget
/// buys exactly one. The flag is derived from the restored count instead.
///
/// The reopen also carries a raised round cap. A turn keeps the cap it was
/// admitted under, so the edit neither refuses the reopen nor buys the resumed
/// turn more rounds.
#[tokio::test]
async fn restored_turn_above_its_round_cap_does_not_wrap_up_twice() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    // Round 1 is the budgeted round and round 2 is the wrap-up; both pause, so
    // the restored checkpoint carries a round count above the cap.
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            2,
            9_300,
            false,
        )
        .with_provider_pauses([0, 1]),
    );
    let store = Arc::new(TestCreateStore::default());
    // One paused round publishes two promoted-hosted commits, so skipping two
    // lands the block on the wrap-up round rather than the budgeted one.
    store.block_after_promoted_hosted_skipping(2);
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let config = AgentConfig::default().with_max_model_rounds(1);
    let original = sessions
        .create(
            "create:restored-wrap-up",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model))
                .config(config),
        )
        .await?;
    let _turn = original.prompt("restore above the round cap").await?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire) && model.calls.load(Ordering::Acquire) >= 2
        {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    // Both rounds ran before the restore: the budgeted one and its wrap-up.
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();

    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open(
            "open:restored-wrap-up",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default().with_max_model_rounds(64)),
        )
        .await?
    else {
        return Err("promoted server pause did not restore as working".into());
    };
    let result = turn.result().result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!(
            "restored wrap-up produced the wrong outcome: {:?}",
            result.outcome()
        )
        .into());
    };
    assert_eq!(
        error.kind(),
        AgentErrorKind::RoundLimit,
        "unexpected restored-wrap-up error: {error:?}"
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2,
        "the restored turn must not run a second wrap-up round"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn assistant_continuation_commits_intermediate_history_usage_and_reopens_without_replay()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 0, 9_400, false)
            .with_assistant_continuations([0])
            .with_report_usage(),
    );
    let agent = sessions
        .create(
            "create:assistant-continuation",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let command_id = CommandId::new("root:assistant-continuation")?;
    let turn = agent
        .start(command_id.clone(), "continue once")
        .await?
        .into_turn()
        .ok_or("root command unexpectedly duplicated")?;
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_exact_terminal_result(&result, &command_id)?;
    assert_eq!(result.checkpoint().manifest().ledger().commands().len(), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(model.observed_request_texts().len(), 2);
    assert!(
        model.observed_request_texts()[1]
            .iter()
            .any(|text| text == "artifact response 0")
    );
    assert_eq!(result.usage().model_operations(), 2);
    assert_eq!(result.usage().model_operations_reporting_usage(), 2);
    assert_eq!(result.usage().model(), Usage::new(6, 4, 0, 0, 0));
    let assistant_text = |result: &TurnResult| {
        result
            .conversation()
            .items()
            .iter()
            .filter(|item| item.actor() == Actor::Assistant)
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        assistant_text(&result),
        ["artifact response 0", "artifact response 1"]
    );
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;

    let reopened_model = artifact_test_model_at(0, 9_500, Timestamp::new(2, 0)?)?;
    let opened = sessions
        .open(
            "open:assistant-continuation",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&reopened_model)),
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("quiescent continuation checkpoint required recovery".into());
    };
    let next = opened.prompt("new root turn").await?.result().await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(reopened_model.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        assistant_text(&next),
        [
            "artifact response 0",
            "artifact response 1",
            "artifact response 0"
        ]
    );
    opened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn repeated_assistant_continuation_stops_at_the_wrap_up_round() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 0, 9_600, false)
            .with_assistant_continuations([0, 1, 2]),
    );
    let agent = artifact_test_builder(workspace, Arc::clone(&model))
        .config(AgentConfig::default().with_max_model_rounds(2))
        .build()?;
    let result = agent.prompt("bounded continuation").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err("repeated continuation exceeded round budget without failing".into());
    };
    assert_eq!(error.kind(), AgentErrorKind::RoundLimit);
    assert_eq!(model.calls.load(Ordering::Acquire), 3);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn interrupted_assistant_continuation_keeps_committed_history_and_fences_replay() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 0, 9_750, false)
            .with_assistant_continuations([0]),
    );
    model.held_call.store(1, Ordering::Release);
    let original = sessions
        .create(
            "create:interrupted-assistant-continuation",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let command_id = CommandId::new("root:interrupted-assistant-continuation")?;
    let _turn = original
        .start(command_id.clone(), "continue then lose driver")
        .await?
        .into_turn()
        .ok_or("root command unexpectedly duplicated")?;
    tokio::time::timeout(Duration::from_secs(5), model.held_call_started.notified()).await?;
    let checkpoint = original.checkpoint();
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            ..
        }
    ));
    let committed = checkpoint.manifest().canonical().projection();
    assert!(committed.items().iter().any(|item| {
        item.actor() == Actor::Assistant
            && item.blocks().iter().any(|block| {
                matches!(block, ContentBlock::Text(text) if text.text() == "artifact response 0")
            })
    }));
    let address =
        DurableSessionAddress::new(original.session_id(), original.branch_id(), checkpoint.id());
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.expire_active_lease();
    let reopened = tokio::time::timeout(
        Duration::from_secs(5),
        sessions.open(
            "open:interrupted-assistant-continuation",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        ),
    )
    .await??;
    let DurableRuntimeOpen::Opened(agent) = reopened else {
        return Err("uncertain second request did not reconcile to one terminal".into());
    };
    let terminal = terminal_record_at_checkpoint(agent.checkpoint().as_ref(), &command_id)?;
    let DurableTerminalOutcome::Cancelled(error) = terminal.outcome else {
        return Err("uncertain second request did not cancel the original turn".into());
    };
    let error = error.into_error()?;
    assert_eq!(error.kind(), AgentErrorKind::Session);
    assert_eq!(
        error.message(),
        "the turn was abandoned after its model request's outcome became uncertain"
    );
    assert!(matches!(
        agent.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(agent.checkpoint().manifest().ledger().commands().len(), 1);
    let assistant_texts = agent
        .conversation()
        .items()
        .iter()
        .filter(|item| item.actor() == Actor::Assistant)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_texts, ["artifact response 0"]);
    agent.shutdown().await?;
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    Ok(())
}

#[tokio::test]
async fn assistant_continuation_follow_up_honors_cancellation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 0, 9_700, false)
            .with_assistant_continuations([0]),
    );
    model.held_call.store(1, Ordering::Release);
    let agent = artifact_test_builder(workspace, Arc::clone(&model)).build()?;
    let turn = agent.prompt("cancel follow-up").await?;
    tokio::time::timeout(Duration::from_secs(5), model.held_call_started.notified()).await?;
    assert_eq!(
        turn.control().cancel().await?,
        CancellationStatus::Requested
    );
    let result = tokio::time::timeout(Duration::from_secs(5), turn.result()).await??;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert!(model.held_call_joined.load(Ordering::Acquire));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn repeated_provider_server_pause_obeys_root_round_limit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    // Every round pauses, the budgeted two and the wrap-up round the budget
    // buys: a wrap-up that never answers cannot extend the turn.
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 3, 9_200, false).with_provider_pauses([0, 1, 2]),
    );
    let agent = artifact_test_builder(workspace, Arc::clone(&model))
        .config(AgentConfig::default().with_max_model_rounds(2))
        .build()?;
    let result = agent.prompt("bounded server pause").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!(
            "round-limited pause turn did not fail: {:?}",
            result.outcome()
        )
        .into());
    };
    assert_eq!(
        error.kind(),
        AgentErrorKind::RoundLimit,
        "unexpected round-limit error: {error:?}"
    );
    // Two budgeted rounds plus the wrap-up round the budget now buys; the
    // wrap-up pauses again instead of answering, so the turn still fails.
    assert_eq!(model.calls.load(Ordering::Acquire), 3);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completed_tool_turn_keeps_artifact_and_prefix_after_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let producer = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            2,
            19_000,
            false,
        )
        .with_tool_first(),
    );
    let builder = |model: Arc<ArtifactModel>| -> TestResult<AgentBuilder> {
        let registry = one_tool_registry()?;
        let configured = configured_test_model(
            model.clone(),
            model.descriptor.clone(),
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        Ok(AgentBuilder::new(configured, registry, workspace.clone())
            .tool_policy(Arc::new(AlwaysAllowPolicy))
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            ))
    };
    let first = sessions
        .create("create:tool-history", builder(producer.clone())?)
        .await?;
    let result = first
        .prompt("use probe then finish")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    let manifest = result.checkpoint().manifest();
    let artifact = manifest.family_artifact().ok_or("artifact")?.clone();
    let before = manifest.canonical().projection();
    let covered_count = before.items().len();
    let prefix = before.canonical_item_prefix_identity(covered_count)?;
    let call = before
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
        .ok_or("call")?;
    assert_eq!(
        call.state(),
        ToolCallState::Requested,
        "execution appends results; it never rewrites model output"
    );
    first.shutdown().await?;
    let address = DurableSessionAddress::new(
        first.session_id(),
        first.branch_id(),
        first.checkpoint().id(),
    );
    let consumer = artifact_test_model_at(1, 20_000, Timestamp::new(2, 0)?)?;
    let DurableRuntimeOpen::Opened(opened) = sessions
        .open("open:tool-history", address, builder(consumer.clone())?)
        .await?
    else {
        return Err("expected quiescent reopen".into());
    };
    let next = opened.prompt("follow up").await?.result().await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(consumer.observed()[0], Some(artifact.to_active()?));
    assert_eq!(
        next.checkpoint()
            .manifest()
            .canonical()
            .projection()
            .canonical_item_prefix_identity(covered_count)?,
        prefix
    );
    let mut protected = next.clone();
    protected.outcome = TurnResultOutcome::Failed(AgentError::from_model(ModelError::from(
        ModelPreparationError::rejected(
            ModelPreparationErrorKind::ProtectedHistory,
            "native history has no compatible replay target",
        ),
    )));
    assert_eq!(
        protected.context_overflow_remedy(),
        None,
        "changing an in-memory outcome does not rewrite the recorded remedy"
    );
    opened.shutdown().await?;
    Ok(())
}

/// Owners of every tool call and result in committed history, in order.
fn committed_tool_owners<'a>(
    items: impl IntoIterator<Item = &'a ConversationItem>,
) -> Vec<(&'static str, ToolExecutionOwner)> {
    items
        .into_iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(("call", call.owner().clone())),
            ContentBlock::ToolResult(result) => Some(("result", result.owner().clone())),
            _ => None,
        })
        .collect()
}

/// The frozen registry's owner class is stamped onto the landed call and its
/// result before the round commits, whatever owner the provider emitted, and
/// committed history keeps both across reopen. The producing round carries no
/// family artifact: an artifact binds the owner the provider emitted (see
/// `protocol_owner_matching_the_registry_keeps_the_family_artifact`).
#[tokio::test]
async fn mcp_owned_registration_commits_mcp_calls_and_results_that_survive_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let producer = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            19_500,
            false,
        )
        .with_tool_first(),
    );
    let owner = plakous_tools::ToolOwnerClass::Mcp(plakous_tools::McpServerName::new("github")?);
    let mcp = owner.execution_owner();
    let builder = |model: Arc<ArtifactModel>| -> TestResult<AgentBuilder> {
        let registry = one_tool_registry_owned_by(owner.clone())?;
        let configured = configured_test_model(
            model.clone(),
            model.descriptor.clone(),
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        Ok(AgentBuilder::new(configured, registry, workspace.clone())
            .tool_policy(Arc::new(AlwaysAllowPolicy))
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            ))
    };
    let first = sessions
        .create("create:mcp-owner", builder(producer.clone())?)
        .await?;
    let result = first
        .prompt("use probe then finish")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    let committed = result.checkpoint().manifest().canonical().projection();
    let expected = vec![("call", mcp.clone()), ("result", mcp.clone())];
    assert_eq!(committed_tool_owners(committed.items().iter()), expected);
    let covered_count = committed.items().len();
    first.shutdown().await?;

    let address = DurableSessionAddress::new(
        first.session_id(),
        first.branch_id(),
        first.checkpoint().id(),
    );
    let consumer = artifact_test_model_at(1, 20_500, Timestamp::new(2, 0)?)?;
    let DurableRuntimeOpen::Opened(opened) = sessions
        .open("open:mcp-owner", address, builder(consumer.clone())?)
        .await?
    else {
        return Err("expected quiescent reopen".into());
    };
    let next = opened.prompt("follow up").await?.result().await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    let reopened = next.checkpoint().manifest().canonical().projection();
    assert_eq!(
        committed_tool_owners(reopened.items().iter().take(covered_count)),
        expected
    );
    opened.shutdown().await?;
    Ok(())
}

/// A protocol configured with the registry's owner emits it at parse time, so
/// landing changes nothing the family artifact's covered projection binds and
/// the artifact round commits with MCP-owned history.
#[tokio::test]
async fn protocol_owner_matching_the_registry_keeps_the_family_artifact() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let owner = plakous_tools::ToolOwnerClass::Mcp(plakous_tools::McpServerName::new("github")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let mut model = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        2,
        19_700,
        false,
    )
    .with_tool_first();
    model.tool_owner = owner.execution_owner();
    let model = Arc::new(model);
    let registry = one_tool_registry_owned_by(owner.clone())?;
    let configured = configured_test_model(
        model.clone(),
        model.descriptor.clone(),
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;
    let result = agent
        .prompt("use probe then finish")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected outcome: {:?}",
        result.outcome()
    );
    let manifest = result.checkpoint().manifest();
    assert!(manifest.family_artifact().is_some());
    assert_eq!(
        committed_tool_owners(manifest.canonical().projection().items().iter()),
        vec![
            ("call", owner.execution_owner()),
            ("result", owner.execution_owner())
        ]
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn family_artifact_reopens_feeds_next_turn_replaces_and_clears_atomically() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store.lose_commit_ack_on(7);
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;

    let producing_model = artifact_test_model(1, 10_000)?;
    let first = sessions
        .create(
            "create:family-artifact",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&producing_model)),
        )
        .await?;
    let first_result = first.prompt("turn one").await?.result().await?;
    assert!(matches!(
        first_result.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let first_artifact = first_result
        .checkpoint()
        .manifest()
        .family_artifact()
        .cloned()
        .ok_or("turn one omitted its family artifact")?;
    let first_affinity = first_result
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .affinity()
        .identity();
    assert_eq!(producing_model.observed(), vec![None]);
    assert_eq!(
        producing_model.observed_affinities()[0]
            .as_ref()
            .map(ModelCacheAffinity::identity),
        Some(first_affinity)
    );
    assert_eq!(producing_model.prepares.load(Ordering::Acquire), 1);
    assert_eq!(producing_model.calls.load(Ordering::Acquire), 1);
    assert_eq!(store.lost_commit_acks.load(Ordering::Relaxed), 1);
    assert!(store.recover_commit_calls.load(Ordering::Relaxed) >= 1);
    assert_eq!(first_artifact.family(), "responses");
    assert_eq!(
        first_artifact.schema_revision(),
        crate::internal::RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION
    );
    assert_eq!(
        first_artifact.codec_revision(),
        crate::internal::RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION
    );
    first.shutdown().await?;
    let address = DurableSessionAddress::new(
        first.session_id(),
        first.branch_id(),
        first.checkpoint().id(),
    );

    let reopening_model = artifact_test_model_at(1, 20_000, Timestamp::new(2, 0)?)?;
    let opened = sessions
        .open(
            "open:family-artifact",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&reopening_model)),
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("quiescent artifact checkpoint unexpectedly required recovery".into());
    };
    let second_result = opened.prompt("turn two").await?.result().await?;
    assert!(matches!(
        second_result.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let second_stored = store
        .load_checkpoint(
            second_result
                .checkpoint()
                .manifest()
                .topology()
                .checkpoint_id,
        )
        .await?;
    let second_parent = store
        .load_checkpoint(
            second_result
                .checkpoint()
                .manifest()
                .topology()
                .parent_checkpoint_id
                .ok_or("reopened turn successor omitted its exact parent")?,
        )
        .await?;
    assert_eq!(
        second_stored.bundle().base_root(),
        Some(second_parent.reference().root())
    );
    assert!(
        second_stored.bundle().stats().new_object_count()
            < second_stored.bundle().stats().object_count()
    );
    assert_eq!(
        second_stored.bundle().stats().new_object_count(),
        second_stored.bundle().supplied_objects().len()
    );
    let observed = reopening_model.observed();
    assert_eq!(
        reopening_model.observed_affinities()[0]
            .as_ref()
            .map(ModelCacheAffinity::identity),
        Some(first_affinity)
    );
    let active = observed
        .first()
        .and_then(Option::as_ref)
        .ok_or("reopened model call omitted the active family artifact")?;
    assert_eq!(active.family(), first_artifact.family());
    assert_eq!(active.schema_revision(), first_artifact.schema_revision());
    assert_eq!(active.codec_revision(), first_artifact.codec_revision());
    assert_eq!(active.binding(), first_artifact.binding());
    assert_eq!(active.bytes(), first_artifact.bytes());
    let second_artifact = second_result
        .checkpoint()
        .manifest()
        .family_artifact()
        .cloned()
        .ok_or("turn two omitted its replacement family artifact")?;
    assert_ne!(second_artifact.binding(), first_artifact.binding());
    assert_ne!(second_artifact.bytes(), first_artifact.bytes());
    let provider = second_result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .rev()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::Turn(turn_id),
                    ..
                }
                    if *turn_id == second_result.turn_id()
            )
        })
        .ok_or("turn two omitted its provider operation")?;
    let prepared_record: PreparedCallRecord = serde_json::from_slice(provider.request().bytes())?;
    assert_eq!(
        prepared_record.projection().active_artifact(),
        Some(first_artifact.to_active()?.integrity())
    );
    let persisted_response = provider
        .transitions()
        .iter()
        .find_map(|transition| match transition.state() {
            DurableOperationState::Completed(component) => {
                crate::runtime::operation_record::ProviderOutcome::decode(component)
                    .ok()
                    .flatten()
                    .and_then(|outcome| outcome.response().cloned())
            }
            DurableOperationState::Prepared
            | DurableOperationState::DecisionRequired(_)
            | DurableOperationState::ContinuationRequired(_)
            | DurableOperationState::Claimed(_)
            | DurableOperationState::Dispatched
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::ProvenNotDispatched(_)
            | DurableOperationState::OutcomeUnknown(_)
            | DurableOperationState::Finalized(_) => None,
        })
        .ok_or("provider operation omitted its reconstructive completed response")?;
    assert_eq!(
        persisted_response
            .family_artifact_candidate()
            .map(SealedFamilyArtifactCandidate::binding),
        Some(second_artifact.binding())
    );
    assert!(!format!("{second_artifact:?}").contains("responses-v4-artifact"));

    let third_result = opened.prompt("turn three").await?.result().await?;
    assert!(matches!(
        third_result.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let observed = reopening_model.observed();
    let active = observed
        .get(1)
        .and_then(Option::as_ref)
        .ok_or("third model call omitted turn two's active artifact")?;
    assert_eq!(active.binding(), second_artifact.binding());
    assert_eq!(active.bytes(), second_artifact.bytes());
    assert_eq!(reopening_model.prepares.load(Ordering::Acquire), 2);
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 2);
    assert!(reopening_model.authority_checks.load(Ordering::Acquire) >= 4);
    assert!(
        third_result
            .checkpoint()
            .manifest()
            .family_artifact()
            .is_none()
    );
    opened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_reopen_accepts_only_monotonic_credential_refresh() -> TestResult {
    for generation in [51, 52, 53, 54] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let descriptor = durable_test_descriptor("test.route")?;
        let original_model = Arc::new(ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 52)?,
            0,
            30_000,
            false,
        ));
        let original = sessions
            .create(
                "create:credential-refresh",
                durable_artifact_test_builder(Arc::clone(&workspace), original_model),
            )
            .await?;
        let prefix = original.prompt("preserved prefix").await?.result().await?;
        assert!(matches!(prefix.outcome(), TurnResultOutcome::Completed(_)));
        let checkpoint = original.checkpoint();
        let address = DurableSessionAddress::new(
            original.session_id(),
            original.branch_id(),
            checkpoint.id(),
        );
        original.shutdown().await?;
        let descriptor = durable_test_descriptor_at("test.route", Timestamp::new(2, 0)?)?;
        let authority = artifact_authority(&descriptor, generation)?;
        let refreshed = RuntimeModelAuthority::new(
            &descriptor,
            authority.account_binding(),
            authority.credential_authority_binding(),
            generation,
            Timestamp::new(21, 0)?,
            Some(Timestamp::new(30, 0)?),
        )?;
        let reopening_model = Arc::new(ArtifactModel::new(descriptor, refreshed, 0, 40_000, false));
        let reopened = sessions
            .open(
                "open:credential-refresh",
                address,
                durable_artifact_test_builder(workspace, Arc::clone(&reopening_model))
                    .identity_sources(
                        Arc::new(FixedClock::new(Timestamp::new(21, 0)?)),
                        Arc::new(SystemIdGenerator),
                    ),
            )
            .await;
        if generation < 52 {
            assert!(reopened.is_err(), "credential rollback must fail closed");
        } else {
            let DurableRuntimeOpen::Opened(reopened) = reopened? else {
                return Err("quiescent refresh unexpectedly required recovery".into());
            };
            assert_eq!(reopened.checkpoint(), checkpoint);
            reopened.shutdown().await?;
        }
        assert_eq!(reopening_model.prepares.load(Ordering::Acquire), 0);
        assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    }
    Ok(())
}

#[tokio::test]
async fn durable_reopen_rejects_refreshed_foreign_authority_and_stale_evidence() -> TestResult {
    for drift in [
        "account",
        "credential",
        "route",
        "observation",
        "future",
        "expiry",
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let original = sessions
            .create(
                "create:foreign-refresh",
                durable_artifact_test_builder(
                    Arc::clone(&workspace),
                    artifact_test_model(0, 30_000)?,
                ),
            )
            .await?;
        let address = DurableSessionAddress::new(
            original.session_id(),
            original.branch_id(),
            original.checkpoint().id(),
        );
        original.shutdown().await?;
        let descriptor = durable_test_descriptor(if drift == "route" {
            "foreign.route"
        } else {
            "test.route"
        })?;
        let prior = artifact_authority(&descriptor, 2)?;
        let foreign = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"foreign-authority"])?;
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            if drift == "account" {
                foreign
            } else {
                prior.account_binding()
            },
            if drift == "credential" {
                foreign
            } else {
                prior.credential_authority_binding()
            },
            2,
            Timestamp::new(
                match drift {
                    "observation" => 8,
                    "future" => 11,
                    _ => 9,
                },
                0,
            )?,
            Some(Timestamp::new(if drift == "expiry" { 10 } else { 20 }, 0)?),
        )?;
        let model = Arc::new(ArtifactModel::new(descriptor, authority, 0, 40_000, false));
        assert!(
            sessions
                .open(
                    "open:foreign-refresh",
                    address,
                    durable_artifact_test_builder(workspace, Arc::clone(&model)),
                )
                .await
                .is_err(),
            "unexpected reopen for {drift}"
        );
        assert_eq!(model.prepares.load(Ordering::Acquire), 0);
        assert_eq!(model.calls.load(Ordering::Acquire), 0);
    }
    Ok(())
}

#[tokio::test]
async fn durable_reopen_rejects_malformed_admission_evidence() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let builder = durable_artifact_test_builder(workspace, artifact_test_model(0, 30_000)?);
    let fact = sealed_runtime_admission(&builder).await?.fact;
    let original: serde_json::Value = serde_json::from_slice(fact.evidence().bytes())?;
    assert!(recorded_admission_authority(&fact, &builder.provider_configuration).is_ok());
    let wrong_reference = crate::RevalidationFact::new(
        BindingReference::new(fact.reference().authority(), fact.reference().subject(), 2)?,
        fact.observed_at(),
        fact.valid_until(),
        fact.evidence().clone(),
    )?;
    assert!(
        recorded_admission_authority(&wrong_reference, &builder.provider_configuration).is_err()
    );
    for field in 0..9 {
        let mut tampered = original.clone();
        if field == 8 {
            tampered
                .as_array_mut()
                .ok_or("admission tuple")?
                .push(serde_json::Value::Null);
        } else {
            tampered[field] = match field {
                0 => serde_json::json!("foreign-admission-domain"),
                1..=5 => original[if field == 4 { 5 } else { 4 }].clone(),
                6 => serde_json::json!(2),
                _ => serde_json::json!(true),
            };
        }
        let poisoned = crate::RevalidationFact::new(
            fact.reference().clone(),
            fact.observed_at(),
            fact.valid_until(),
            runtime_component(
                RuntimeComponentKind::AdmissionEvidence,
                serde_json::to_vec(&tampered)?,
            )?,
        )?;
        assert!(
            recorded_admission_authority(&poisoned, &builder.provider_configuration).is_err(),
            "accepted field {field}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn durable_duplicate_reopens_exact_historical_terminal_without_redispatch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let producing_model = artifact_test_model(2, 30_000)?;
    let agent = sessions
        .create(
            "create:terminal-result-reopen",
            durable_artifact_test_builder(Arc::clone(&workspace), producing_model),
        )
        .await?;
    let command_id = CommandId::new("product:terminal-result:1")?;
    let first = agent
        .start(command_id.clone(), "first terminal result")
        .await?
        .into_turn()
        .ok_or("accepted first turn omitted its turn")?
        .result()
        .await?;
    let first_checkpoint = first.checkpoint().id();
    let first_conversation = first.conversation().clone();
    let second = agent
        .prompt("advance the durable head")
        .await?
        .result()
        .await?;
    assert_ne!(second.checkpoint().id(), first_checkpoint);
    let predecessor_cursor = agent
        .attach(ViewRequest::metadata())
        .await?
        .snapshot()
        .cursor();
    agent.shutdown().await?;
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );

    let reopening_model = artifact_test_model(0, 40_000)?;
    let opened = sessions
        .open(
            "open:terminal-result-reopen",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&reopening_model)),
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("quiescent terminal head unexpectedly required recovery".into());
    };
    assert!(matches!(
        opened
            .resume(predecessor_cursor, ViewRequest::metadata())
            .await,
        Err(ViewError::IncompatibleCursor)
    ));
    let commits_before_attach = store.commit_calls.load(Ordering::Acquire);
    let attachment = opened.attach(ViewRequest::content()).await?;
    assert_eq!(
        attachment.snapshot().committed_conversation_items(),
        second.conversation().items().len()
    );
    assert!(
        attachment
            .snapshot()
            .conversation()
            .is_some_and(|items| !items.is_empty())
    );
    let cursor = attachment.snapshot().cursor();
    drop(attachment);
    assert!(matches!(
        opened.resume(cursor, ViewRequest::content()).await,
        Ok(ResumeOutcome::Replaying(_))
    ));
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits_before_attach,
        "attach/resume must not create semantic-store transactions"
    );
    let duplicate = opened
        .start(command_id.clone(), "first terminal result")
        .await?;
    assert!(duplicate.turn().is_none());
    let duplicate = duplicate
        .into_result()
        .ok_or("reopened duplicate omitted its result handle")?
        .result()
        .await?;
    assert_eq!(duplicate.turn_id(), first.turn_id());
    assert_eq!(duplicate.checkpoint().id(), first_checkpoint);
    assert_eq!(duplicate.outcome(), first.outcome());
    assert_eq!(duplicate.usage(), first.usage());
    assert_eq!(duplicate.conversation(), &first_conversation);
    assert_eq!(reopening_model.prepares.load(Ordering::Acquire), 0);
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);

    let conflict = opened
        .command(TurnCommand::Start {
            command_id,
            text: "conflicting terminal result".to_owned(),
        })
        .await?;
    assert!(conflict.acknowledgement().rejection().is_some());
    assert!(conflict.result().is_none());
    assert_eq!(reopening_model.prepares.load(Ordering::Acquire), 0);
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    opened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn foreign_family_artifact_coverage_rejects_without_canonical_promotion() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = tampered_artifact_test_model(30_000)?;
    let agent = artifact_test_builder(workspace, model).build()?;
    let result = agent.prompt("tampered artifact").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err("foreign family artifact unexpectedly committed".into());
    };
    assert_eq!(error.kind(), AgentErrorKind::Model);
    let items = result.conversation().items();
    assert_eq!(items.len(), 2);
    assert!(text_of(&items[1]).starts_with("the turn handling the request above failed ("));
    assert!(result.checkpoint().manifest().family_artifact().is_none());
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
            .filter(|record| operation_turn_id(record) == Some(result.turn_id()))
            .all(|record| matches!(record.state(), DurableOperationState::Finalized(_)))
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn maintained_model_caller_envelope_promotes_and_reopens_exact_family_artifact() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let caller = custom_artifact_test_descriptor("test.route")?;
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        caller.protocol().clone(),
        caller.route().clone(),
        caller.requested_model().clone(),
        caller.reported_model().cloned(),
        caller.model_family().cloned(),
        caller.profile().clone(),
        caller.capabilities().clone(),
    )
    .with_context_contract(caller.context_contract().ok_or("caller envelope")?.clone());
    assert!(matches!(
        descriptor
            .context_contract()
            .ok_or("context")?
            .capacity()
            .evidence_class(),
        plakous_model::ContextEvidenceClass::CustomEnvelope { .. }
    ));
    assert!(
        descriptor
            .preflight_context_admission_requirement(
                ContextAdmissionRequirement::ReleaseVerifiedRequired
            )
            .is_err()
    );
    let authority = artifact_authority(&descriptor, 1)?;
    let producing = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        authority.clone(),
        1,
        31_000,
        false,
    ));
    let first = sessions
        .create(
            "create:maintained-caller-envelope",
            durable_artifact_test_builder(workspace.clone(), producing.clone()),
        )
        .await?;
    let result = first
        .prompt("publish maintained artifact under caller input envelope")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let artifact = result
        .checkpoint()
        .manifest()
        .family_artifact()
        .cloned()
        .ok_or("managed artifact was not promoted")?;
    assert_eq!(producing.observed(), vec![None]);
    assert_eq!(producing.calls.load(Ordering::Acquire), 1);
    let affinity = result
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .affinity()
        .identity();
    first.shutdown().await?;
    let address = DurableSessionAddress::new(
        first.session_id(),
        first.branch_id(),
        first.checkpoint().id(),
    );
    let reopening = Arc::new(ArtifactModel::new(descriptor, authority, 1, 32_000, false));
    let reopened = sessions
        .open(
            "open:maintained-caller-envelope",
            address,
            durable_artifact_test_builder(workspace, reopening.clone()),
        )
        .await?;
    let DurableRuntimeOpen::Opened(reopened) = reopened else {
        return Err("quiescent reopen requires recovery".into());
    };
    let result = reopened
        .prompt("reuse the exact maintained artifact")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let observed = reopening.observed();
    let active = observed
        .first()
        .and_then(Option::as_ref)
        .ok_or("reopen omitted exact active artifact")?;
    assert_eq!(active.binding(), artifact.binding());
    assert_eq!(active.bytes(), artifact.bytes());
    assert_eq!(
        reopening.observed_affinities()[0]
            .as_ref()
            .map(ModelCacheAffinity::identity),
        Some(affinity)
    );
    assert!(result.checkpoint().manifest().family_artifact().is_some());
    assert_eq!(reopening.calls.load(Ordering::Acquire), 1);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn custom_model_artifact_candidate_rejects_without_promotion_or_continuation_reuse()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = custom_artifact_test_descriptor("test.custom-route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        authority.clone(),
        1,
        30_500,
        false,
    ));
    let registry = ToolRegistryBuilder::default().freeze();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;

    let rejected = agent
        .prompt("reject custom artifact")
        .await?
        .result()
        .await?;
    let TurnResultOutcome::Failed(error) = rejected.outcome() else {
        return Err("custom family artifact unexpectedly committed".into());
    };
    assert_eq!(error.kind(), AgentErrorKind::Model);
    assert!(rejected.checkpoint().manifest().family_artifact().is_none());
    assert_eq!(model.observed(), vec![None]);
    let observed_affinity = model
        .observed_affinities()
        .into_iter()
        .next()
        .flatten()
        .ok_or("custom artifact prepare omitted cache affinity")?
        .identity();
    assert_eq!(model.produced_affinities(), vec![Some(observed_affinity)]);

    let next = agent
        .prompt("continue without rejected artifact")
        .await?
        .result()
        .await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    assert!(next.checkpoint().manifest().family_artifact().is_none());
    assert_eq!(model.prepares.load(Ordering::Acquire), 2);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(model.observed(), vec![None, None]);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn foreign_family_artifact_affinity_rejects_without_canonical_promotion() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = foreign_affinity_artifact_test_model(31_000)?;
    let agent = artifact_test_builder(workspace, Arc::clone(&model)).build()?;
    let result = agent
        .prompt("foreign affinity artifact")
        .await?
        .result()
        .await?;
    let observed = model
        .observed_affinities()
        .into_iter()
        .next()
        .flatten()
        .ok_or("runtime omitted its cache affinity")?
        .identity();
    let produced = model
        .produced_affinities()
        .into_iter()
        .next()
        .flatten()
        .ok_or("fixture omitted its candidate cache affinity")?;
    assert_ne!(observed, produced);
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err("foreign-affinity family artifact unexpectedly committed".into());
    };
    assert_eq!(error.kind(), AgentErrorKind::Model);
    let items = result.conversation().items();
    assert_eq!(items.len(), 2);
    assert!(text_of(&items[1]).starts_with("the turn handling the request above failed ("));
    assert!(result.checkpoint().manifest().family_artifact().is_none());
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_or_unowned_model_authority_fails_closed_and_is_redacted() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account-secret"])?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        account,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential-secret"])?,
        1,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(2, 0)?),
    )?;
    let debug = format!("{authority:?}");
    assert!(!debug.contains(&lowercase_hex(account.bytes())));
    let stale_model: Arc<dyn LanguageModel> = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone()),
    );
    let stale_registry = ToolRegistryBuilder::default().freeze();
    let stale = AgentBuilder::new(
        configured_test_model(
            stale_model,
            descriptor.clone(),
            authority.clone(),
            &stale_registry,
            LocalDurabilityDisposition::Ephemeral,
            ContextAdmissionRequirement::AnyBounded,
        )?,
        stale_registry,
        Arc::clone(&workspace),
    )
    .identity_sources(
        Arc::new(FixedClock::new(Timestamp::new(3, 0)?)),
        Arc::new(SystemIdGenerator),
    );
    let stale_error = sealed_runtime_admission(&stale)
        .await
        .err()
        .ok_or("stale authority was admitted")?;
    assert_eq!(stale_error.kind(), AgentErrorKind::InvalidInput);
    assert!(stale_error.message().contains("authority is stale"));
    assert!(!stale_error.message().contains("account-secret"));
    assert!(!stale_error.message().contains("credential-secret"));
    let unavailable_model: Arc<dyn LanguageModel> = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), []),
    );
    let unavailable_registry = ToolRegistryBuilder::default().freeze();
    let unavailable = AgentBuilder::new(
        configured_test_model(
            unavailable_model,
            descriptor,
            authority,
            &unavailable_registry,
            LocalDurabilityDisposition::Ephemeral,
            ContextAdmissionRequirement::AnyBounded,
        )?,
        unavailable_registry,
        workspace,
    );
    let unavailable_error = sealed_runtime_admission(&unavailable)
        .await
        .err()
        .ok_or("unavailable authority was admitted")?;
    assert_eq!(unavailable_error.kind(), AgentErrorKind::InvalidInput);
    assert!(
        unavailable_error
            .message()
            .contains("authority is unavailable")
    );
    assert_ne!(stale_error.message(), unavailable_error.message());
    Ok(())
}
