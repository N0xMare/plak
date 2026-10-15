//! Live driver paths that must settle without recovery.
use super::*;

/// A cell that yielded earlier in the batch is still live when a later sibling
/// fails operationally. Its marker holds the yielded result, so the batch
/// closes with that result, the turn fails with the sibling's error, and the
/// root boundary terminates the cell; nothing is left for recovery.
#[tokio::test]
async fn sibling_failure_after_a_yielded_cell_fails_without_recovery() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5060, false)
            .with_scripted_tool_calls([(0, "exec", serde_json::to_string("yield once")?)])
            .with_second_direct_tool_named("missing_tool")?,
    );
    let composition_descriptor =
        neutral_composition_for(&model, &descriptor, &authority, &registry)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = composition_test_builder(
        workspace,
        registry,
        &model,
        descriptor,
        authority,
        composition_descriptor,
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::RootTurnJoined,
        ),
    )?;
    let store = Arc::new(TestCreateStore::default());
    store
        .fail_second_effect_preparation
        .store(true, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:yielded-cell-sibling-failure", builder)
        .await?;
    let command_id = CommandId::new("root:yielded-cell-sibling-failure")?;
    let result = agent
        .start(command_id.clone(), "yield, then fail a sibling")
        .await?
        .into_turn()
        .ok_or("root was not admitted")?
        .result()
        .await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!(
            "a sibling failure after a yielded cell ended {:?}\n{}",
            result.outcome(),
            describe_ledger(&result)
        )
        .into());
    };
    assert_eq!(error.kind(), AgentErrorKind::Session);
    assert_exact_terminal_result(&result, &command_id)?;
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(evidence.executes.load(Ordering::Acquire), 1);
    assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
    let results = result
        .conversation()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2, "{}", describe_ledger(&result));
    assert_eq!(results[0].kind(), ToolResultKind::Success);
    assert_eq!(results[1].kind(), ToolResultKind::Failure);
    assert!(
        result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .iter()
            .filter(|record| matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
            ))
            .all(|record| matches!(record.state(), DurableOperationState::Finalized(_))),
        "{}",
        describe_ledger(&result)
    );
    let _ = agent.shutdown().await;
    Ok(())
}

/// A store failure while accepting an operator decision must leave the
/// decision answerable: the retry is accepted and the effect runs, instead of
/// the retry being refused as stale and the turn waiting out its expiry.
#[tokio::test]
async fn live_decision_is_answerable_after_an_acceptance_write_fails() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            97_260,
            false,
        )
        .with_tool_first(),
    );
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:retried-approval",
            durable_decision_builder(workspace, model, one_mutating_tool_registry()?)?,
        )
        .await?;
    let mut turn = agent.prompt("approve after a failed write").await?;
    let mut events = turn.take_events()?;
    let decision = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await?;
            if let AgentEventKind::EffectDecisionRequired { decision, .. } = event.kind() {
                break Ok::<_, Box<dyn std::error::Error>>(decision.clone());
            }
        }
    })
    .await??;
    *lock_mutex(&store.reject_next_commit) = Some(SessionStoreError::BackendUnavailable);
    let first = turn
        .control()
        .allow_effect(
            decision.clone(),
            EffectDecisionAuditId::new("test:retried-approval:1")?,
        )
        .await;
    assert!(first.is_err(), "{first:?}");
    let retry = turn
        .control()
        .allow_effect(
            decision,
            EffectDecisionAuditId::new("test:retried-approval:2")?,
        )
        .await?;
    assert_eq!(retry, EffectDecisionStatus::Allowed);
    let result = tokio::time::timeout(Duration::from_secs(10), turn.result()).await??;
    agent.shutdown().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}\n{}",
        result.outcome(),
        describe_ledger(&result)
    );
    Ok(())
}

/// Recurring oversized output re-arms pressure after each committed fold, so
/// without a per-turn cap a turn could pay one summary per tool group up to its
/// round limit. Past the cap only the free placeholder rung acts.
#[tokio::test]
async fn recurring_oversized_output_caps_committed_paid_folds_per_turn() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut bodies = vec![20_000];
    bodies.extend([60_000; 20]);
    bodies.push(2_000);
    let (builder, model) = placeholder_fixture(
        workspace,
        338_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    let agent = builder.build()?;
    let result = agent
        .prompt("keep reading large files")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), bodies.len());
    assert_eq!(
        model.paid_compaction_calls.load(Ordering::Acquire),
        3,
        "rounds={:?}",
        *lock_mutex(&model.paid_compaction_rounds)
    );
    agent.shutdown().await?;
    Ok(())
}
