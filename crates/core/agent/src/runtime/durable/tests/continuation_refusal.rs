use super::*;

/// A provider-programmatic tool-first model over a 40k window whose next
/// round becomes over capacity once a tool result is in the request.
fn capacity_refusing_model(entry_base: u128) -> TestResult<Arc<ArtifactModel>> {
    let descriptor = placeholder_test_descriptor(40_000, false)?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            entry_base,
            false,
        )
        .with_tool_first()
        .with_programmatic_local_tool()?,
    );
    model
        .tool_successor_capacity_loss
        .store(40_000, Ordering::Release);
    Ok(model)
}

fn sessions(store: &Arc<TestCreateStore>) -> TestResult<DurableSessionRuntime> {
    Ok(DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?)
}

/// The turn failed with the typed capacity refusal, the head is Quiescent and
/// the one continuation of `continuation`'s kind is Finalized.
fn assert_capacity_refusal_finalized(
    result: &TurnResult,
    continuation: fn(&DurableOperationKind) -> bool,
) -> TestResult {
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!("expected a capacity refusal, got {:?}", result.outcome()).into());
    };
    assert_eq!(
        error.model_kind(),
        Some(ModelErrorKind::ContextLimit),
        "{error:?}"
    );
    let checkpoint = result.checkpoint();
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let continuations = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| continuation(record.kind()))
        .collect::<Vec<_>>();
    assert_eq!(continuations.len(), 1);
    assert!(matches!(
        continuations[0].state(),
        DurableOperationState::Finalized(_)
    ));
    Ok(())
}

fn tool_batch_continuation(kind: &DurableOperationKind) -> bool {
    matches!(kind, DurableOperationKind::ToolBatchContinuation { .. })
}

/// A provider-programmatic batch keeps its continuation open across the
/// tool-batch fence. A next-round capacity refusal there, with scheduled
/// compaction disabled, finalizes that continuation before the turn fails,
/// so the head settles Quiescent with one terminal.
#[tokio::test]
async fn programmatic_tool_batch_capacity_refusal_finalizes_its_continuation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = capacity_refusing_model(0xA300_0000)?;
    let agent = sessions(&Arc::new(TestCreateStore::default()))?
        .create(
            "create:programmatic-capacity-refusal",
            durable_decision_builder(
                workspace,
                Arc::clone(&model),
                mutating_tool_registry_with_results([ToolOutcome::success_text("ok")])?,
            )?
            .tool_policy(Arc::new(AlwaysAllowPolicy))
            .with_scheduled_compaction_enabled(false),
        )
        .await?;
    let result = agent.prompt("run the tool").await?.result().await?;
    assert_capacity_refusal_finalized(&result, tool_batch_continuation)?;
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "no further dispatch"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A reopened provider-programmatic tool-batch frame runs its batch, and the
/// restored fence's capacity refusal finalizes the restored continuation.
#[tokio::test]
async fn restored_programmatic_tool_batch_capacity_refusal_finalizes_its_continuation() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = capacity_refusing_model(0xA310_0000)?;
    let registry = mutating_tool_registry_with_results([ToolOutcome::success_text("ok")])?;
    let builder = || -> TestResult<AgentBuilder> {
        Ok(
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .with_scheduled_compaction_enabled(false),
        )
    };
    let store = Arc::new(TestCreateStore::default());
    store.block_after_hosted_effect_count(0);
    let sessions = sessions(&store)?;
    let original = sessions
        .create("create:restored-batch-capacity-refusal", builder()?)
        .await?;
    let _turn = original.prompt("interrupt hosted tool batch").await?;
    let address = loop {
        if store.commits_blocked.load(Ordering::Acquire) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                original.checkpoint().id(),
            );
        }
        tokio::task::yield_now().await;
    };
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open("open:restored-batch-capacity-refusal", address, builder()?)
        .await?
    else {
        return Err("hosted tool-batch frame did not restore as working".into());
    };
    let result = turn.result().result().await?;
    assert_capacity_refusal_finalized(&result, tool_batch_continuation)?;
    assert_eq!(model.calls.load(Ordering::Acquire), 1, "no provider replay");
    agent.shutdown().await?;
    Ok(())
}

/// A reopened provider-programmatic batch paused at an effect decision runs
/// after approval, and the restored fence's capacity refusal finalizes the
/// hosted continuation that frame carries.
#[tokio::test]
async fn restored_programmatic_decision_capacity_refusal_finalizes_its_continuation() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = capacity_refusing_model(0xA320_0000)?;
    let registry = mutating_tool_registry_with_results([ToolOutcome::success_text("ok")])?;
    let builder = || -> TestResult<AgentBuilder> {
        Ok(
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?
                .with_scheduled_compaction_enabled(false),
        )
    };
    let store = Arc::new(TestCreateStore::default());
    let sessions = sessions(&store)?;
    let original = sessions
        .create("create:restored-decision-capacity-refusal", builder()?)
        .await?;
    let _turn = original.prompt("restore pending effect").await?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open(
            "open:restored-decision-capacity-refusal",
            address,
            builder()?,
        )
        .await?
    else {
        return Err("pending decision did not restore as working".into());
    };
    let decision = turn
        .pending_decision()
        .ok_or("restored effect decision was absent")?
        .clone();
    assert_eq!(
        turn.control()
            .allow_effect(
                decision,
                EffectDecisionAuditId::new("test:restore-capacity-allow")?
            )
            .await?,
        EffectDecisionStatus::Allowed
    );
    let result = turn.result().result().await?;
    assert_capacity_refusal_finalized(&result, tool_batch_continuation)?;
    assert_eq!(model.calls.load(Ordering::Acquire), 1, "no provider replay");
    agent.shutdown().await?;
    let _ = original.shutdown().await;
    Ok(())
}
