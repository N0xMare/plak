use super::*;

#[tokio::test]
async fn provider_overflow_refreshes_expired_paid_fold_backoff() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let bodies = [20_000, 60_000];
    let (builder, model) = placeholder_fixture(
        workspace,
        335_000,
        40_000,
        &bodies,
        &[0, 1],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    model
        .fail_first_compaction_call
        .store(true, Ordering::Release);
    *lock_mutex(&model.assistant_continuation_calls) = BTreeSet::from([3, 4]);
    // Two assistant continuations finalize rounds without a tool fence to refresh the
    // guard. The next provider refusal must refresh it before the paid ladder.
    model.overflow_once_at_call.store(5, Ordering::Release);
    let agent = builder.build()?;
    let mut turn = agent
        .prompt("continue after a failed fold and later overflow")
        .await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    let mut compactions = Vec::new();
    loop {
        match events.recv().await?.kind() {
            AgentEventKind::CompactionFinished {
                outcome, detail, ..
            } => compactions.push(format!("{outcome:?}: {detail:?}")),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}; rounds={}, calls={}, ordinary={}, folds={compactions:?}; paid_rounds={:?}",
        result.outcome(),
        result.model_rounds(),
        model.calls.load(Ordering::Acquire),
        model.ordinary_calls.load(Ordering::Acquire),
        *lock_mutex(&model.paid_compaction_rounds)
    );
    assert_eq!(*lock_mutex(&model.paid_compaction_rounds), [2, 4]);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unpaid_admission_refusal_can_fold_after_follow_up_intent_commits() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        336_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder.build()?;
    assert!(matches!(
        agent
            .prompt("protected intent ".repeat(1_000))
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    // The ordinary envelope grows, while the summary envelope remains small.
    // Before new intent commits the only turn is protected, so preflight is
    // unpaid and unavailable. The new intent makes the old turn foldable.
    model
        .ordinary_context_overhead
        .store(38_000, Ordering::Release);
    let result = agent
        .prompt("new intent creates a legal cut")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn long_turn_recurring_failed_paid_folds_have_a_per_turn_cost_cap() -> TestResult {
    recurring_failed_paid_fold_cost_case(false).await
}

#[tokio::test]
async fn long_turn_recurring_nonshrinking_paid_folds_have_a_per_turn_cost_cap() -> TestResult {
    recurring_failed_paid_fold_cost_case(true).await
}

async fn recurring_failed_paid_fold_cost_case(nonshrinking: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut bodies = vec![20_000];
    bodies.extend([60_000; 20]);
    bodies.push(2_000);
    let (builder, model) = placeholder_fixture(
        workspace,
        334_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    if nonshrinking {
        model.make_compaction_success_non_shrinking(100_000);
    } else {
        model.fail_compaction_call();
    }
    let agent = builder.build()?;
    let result = agent
        .prompt("keep reading despite failed summaries")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), bodies.len());
    assert_eq!(*lock_mutex(&model.paid_compaction_rounds), [2, 4, 8]);
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 3);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn long_turn_rearms_pressure_after_committed_paid_folds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let bodies = [20_000, 20_000, 60_000, 20_000, 20_000, 60_000, 2_000];
    let (builder, model) = placeholder_fixture(
        workspace,
        330_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    let agent = builder.build()?;
    let result = agent
        .prompt("read both large files")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), bodies.len());
    // Each native successor keeps the current round's 60k batch, so the
    // next 20k round crosses the fence again after the first fold.
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 3);
    assert_eq!(*lock_mutex(&model.paid_compaction_rounds), [3, 5, 6]);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn long_turn_paid_failure_backs_off_then_commits_a_new_fold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let bodies = [20_000, 60_000, 60_000, 60_000, 2_000];
    let (builder, model) = placeholder_fixture(
        workspace,
        331_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    model
        .fail_first_compaction_call
        .store(true, Ordering::Release);
    let agent = builder.build()?;
    let result = agent
        .prompt("read the files despite a failed fold")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), bodies.len());
    assert_eq!(*lock_mutex(&model.paid_compaction_rounds), [2, 4]);
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("fold provenance")?
            .strategy(),
        RuntimeCompactionStrategy::NativeResponsesTriggerV1,
        "the later paid fold commits instead of staying on placeholders forever"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn follow_up_admission_compacts_a_full_session_before_refusing() -> TestResult {
    for (enabled, fail_fold) in [(true, false), (false, false), (true, true)] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let (builder, model) = placeholder_fixture(
            workspace,
            332_000,
            40_000,
            &[],
            &[],
            false,
            LocalDurabilityDisposition::Ephemeral,
        )?;
        let agent = builder.with_scheduled_compaction_enabled(enabled).build()?;
        let older = "older intent ".repeat(1_000);
        assert!(matches!(
            agent.prompt(older).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
        assert!(matches!(
            agent
                .prompt("second intent ".repeat(1_000))
                .await?
                .result()
                .await?
                .outcome(),
            TurnResultOutcome::Completed(_)
        ));
        assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
        // Reveal additional token cost in the retained history. Each old turn
        // still fits alone, but the committed source now exceeds the window.
        let source_tokens = agent
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .retained_bytes_upper_bound()
            .div_ceil(4);
        let multiplier = 40_000 / source_tokens + 1;
        assert!(multiplier > 1);
        model
            .context_token_multiplier
            .store(multiplier, Ordering::Release);
        model
            .fail_first_compaction_call
            .store(fail_fold, Ordering::Release);
        let dispatches_before = model.calls.load(Ordering::Acquire);
        let mut turn = agent.prompt("short follow-up").await?;
        let mut events = turn.take_events()?;
        let result = turn.result().await?;
        let mut compactions = Vec::new();
        loop {
            match events.recv().await?.kind() {
                AgentEventKind::CompactionFinished {
                    outcome, detail, ..
                } => compactions.push(format!("{outcome:?}: {detail:?}")),
                AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
                _ => {}
            }
        }
        assert_eq!(compactions.len(), usize::from(enabled));
        if enabled && !fail_fold {
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{:?}; compactions={compactions:?}, paid={}, estimates={:?}",
                result.outcome(),
                model.paid_compaction_calls.load(Ordering::Acquire),
                *lock_mutex(&model.compaction_wire_estimates)
            );
            assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
            assert_eq!(result.model_rounds(), 1);
        } else {
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Failed(error) if error.model_kind() == Some(ModelErrorKind::ContextLimit))
            );
            assert_eq!(
                model.paid_compaction_calls.load(Ordering::Acquire),
                usize::from(fail_fold)
            );
            assert_eq!(
                model.calls.load(Ordering::Acquire),
                dispatches_before + usize::from(fail_fold),
                "an uncommitted admission fold neither dispatches ordinary input nor repeats its ladder"
            );
        }
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn long_turn_success_resets_backoff_but_preserves_the_failure_cap() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut bodies = vec![20_000];
    bodies.extend([60_000; 20]);
    bodies.push(2_000);
    let (builder, model) = placeholder_fixture(
        workspace,
        337_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    *lock_mutex(&model.fail_compaction_calls) = BTreeSet::from([1, 3, 4]);
    let agent = builder.build()?;
    let result = agent
        .prompt("fail, commit, fail twice, then stop paying")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}; rounds={:?}; starts={:?}; ordinary={}",
        result.outcome(),
        *lock_mutex(&model.paid_compaction_rounds),
        *lock_mutex(&model.compaction_start_rounds),
        model.ordinary_calls.load(Ordering::Acquire)
    );
    assert_eq!(result.tool_calls(), bodies.len());
    assert_eq!(*lock_mutex(&model.paid_compaction_rounds), [2, 4, 5, 7]);
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 4);
    let checkpoint = agent.checkpoint();
    let failures = checkpoint
        .manifest()
        .ledger()
        .operations()
        .iter()
        .filter(|record| {
            let DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(workflow),
                ..
            } = record.kind()
            else {
                return false;
            };
            record.transitions().last().is_some_and(|transition| {
                crate::runtime::compaction::record::compaction_checkpoint_id(*workflow, b"failed")
                    .ok()
                    == Some(transition.checkpoint_id())
            })
        })
        .count();
    assert_eq!(failures, 3);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn long_turn_failure_cap_starts_fresh_for_each_root_turn() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut bodies = vec![20_000];
    bodies.extend([60_000; 20]);
    bodies.push(2_000);
    bodies.extend([60_000; 8]);
    let mut rounds = (0..22).collect::<Vec<_>>();
    rounds.extend(23..31);
    let (builder, model) = placeholder_fixture(
        workspace,
        338_000,
        40_000,
        &bodies,
        &rounds,
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    *lock_mutex(&model.fail_compaction_calls) = BTreeSet::from([1, 2, 3, 5]);
    let agent = builder.build()?;
    let first = agent.prompt("spend three failures").await?.result().await?;
    assert!(
        matches!(first.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        first.outcome()
    );
    assert_eq!(first.tool_calls(), 22);
    assert_eq!(*lock_mutex(&model.paid_compaction_rounds), [2, 4, 8]);
    let second = agent
        .prompt("a fresh turn can pay again")
        .await?
        .result()
        .await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        second.outcome()
    );
    assert_eq!(second.tool_calls(), 8);
    // The first failure in the new turn must re-arm despite the prior root's
    // exhausted cap; its next paid fold is eligible after just two rounds.
    assert!(model.paid_compaction_calls.load(Ordering::Acquire) >= 6);
    {
        let paid_rounds = lock_mutex(&model.paid_compaction_rounds);
        assert_eq!(paid_rounds[5] - paid_rounds[4], 2);
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn long_turn_dispatch_rejections_leave_paid_cap_and_backoff_unchanged() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut bodies = vec![20_000];
    bodies.extend([60_000; 20]);
    bodies.push(2_000);
    let (builder, model) = placeholder_fixture(
        workspace,
        339_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    *lock_mutex(&model.reject_compaction_calls) = BTreeSet::from([1, 2, 3]);
    *lock_mutex(&model.fail_compaction_calls) = BTreeSet::from([4, 5, 6]);
    let agent = builder.build()?;
    let result = agent
        .prompt("unpaid rejections do not delay paid folds")
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
        *lock_mutex(&model.compaction_start_rounds),
        [2, 3, 4, 5, 7, 11]
    );
    assert_eq!(*lock_mutex(&model.paid_compaction_rounds), [5, 7, 11]);
    let checkpoint = agent.checkpoint();
    let rejections = checkpoint
        .manifest()
        .ledger()
        .operations()
        .iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(_),
                    ..
                }
            ) && record.transitions().iter().any(|transition| {
                matches!(
                    transition.state(),
                    DurableOperationState::ProvenNotDispatched(_)
                )
            })
        })
        .count();
    assert_eq!(rejections, 3);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn paid_fold_failure_and_placeholder_fallback_events_match_feed_workflows() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let bodies = [20_000, 60_000, 60_000, 60_000, 2_000];
    let (builder, model) = placeholder_fixture(
        workspace,
        336_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.script_tool_rounds_around_compactions();
    model
        .fail_first_compaction_call
        .store(true, Ordering::Release);
    let agent = runtime.create("create:fallback-parity", builder).await?;
    let mut turn = agent.prompt("read despite the failed paid fold").await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let mut trace = Vec::new();
    loop {
        match events.recv().await?.kind() {
            AgentEventKind::CompactionStarted { workflow_id, .. } => {
                trace.push((*workflow_id, None));
            }
            AgentEventKind::CompactionFinished {
                workflow_id,
                outcome,
                ..
            } => trace.push((*workflow_id, Some(*outcome))),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let facts = lock_mutex(&store.feed_records)
        .iter()
        .flat_map(|r| r.facts().to_vec())
        .collect::<Vec<_>>();
    let failed = facts
        .iter()
        .find_map(|fact| match fact {
            crate::feed::FeedFact::WorkflowSettled {
                workflow_id,
                outcome: crate::feed::FeedWorkflowOutcome::Failed { .. },
                ..
            } => Some(*workflow_id),
            _ => None,
        })
        .ok_or("paid failure fact")?;
    let position = trace
        .iter()
        .position(|entry| *entry == (failed, Some(CompactionEventOutcome::Failed)))
        .ok_or("paid failure event")?;
    let (fallback, started) = *trace.get(position + 1).ok_or("fallback start")?;
    assert_ne!(fallback, failed);
    assert_eq!(started, None);
    assert_eq!(
        trace.get(position + 2),
        Some(&(fallback, Some(CompactionEventOutcome::Committed)))
    );
    for (workflow, outcome) in trace {
        if outcome == Some(CompactionEventOutcome::Committed) {
            assert!(facts.iter().any(|fact| matches!(fact, crate::feed::FeedFact::WorkflowSettled { workflow_id, outcome: crate::feed::FeedWorkflowOutcome::Compacted { .. }, .. } if *workflow_id == workflow)), "committed event has no fact: {workflow:?}");
        }
    }
    agent.shutdown().await?;
    Ok(())
}
