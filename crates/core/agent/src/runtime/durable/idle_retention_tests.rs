use super::*;

#[tokio::test]
async fn ready_start_is_admitted_before_blocked_idle_retention() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store
        .block_terminal_at_revision
        .store(64, Ordering::Release);
    store.block_prune.store(true, Ordering::Release);
    let agent = telemetry_durability_test_agent(
        store.clone(),
        Arc::new(TelemetryEmitter::disabled()),
        workspace,
    )
    .await?;
    let terminal = loop {
        let turn = agent.prompt("reach the retention horizon").await?;
        tokio::select! {
            result = turn.result.clone().result() => {
                assert!(matches!(result?.outcome(), TurnResultOutcome::Completed(_)));
            }
            () = store.terminal_started.notified() => break turn,
        }
    };
    let (reply, receiver) = tokio::sync::oneshot::channel();
    agent
        .inner
        .controls
        .try_send(crate::ActorControl::Command {
            command: TurnCommand::Start {
                command_id: CommandId::new("retention:ready-start")?,
                input: "start before idle maintenance".into(),
            },
            reply,
        })?;
    store.terminal_release.notify_one();
    assert!(matches!(
        terminal.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let admitted = tokio::select! {
        biased;
        admitted = receiver => admitted?,
        () = store.prune_started.notified() => panic!("prune dispatched before the ready admission commit"),
    };
    assert_eq!(
        admitted.acknowledgement.disposition(),
        CommandAckDisposition::Accepted
    );
    let result = admitted.result.ok_or("result handle")?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    store.prune_started.notified().await;
    assert_eq!(store.prune_calls.load(Ordering::Acquire), 1);
    store.block_prune.store(false, Ordering::Release);
    store.prune_release.notify_one();
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn queued_roots_settle_before_blocked_idle_retention() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store.block_prune.store(true, Ordering::Release);
    let model = artifact_test_model(0, 99_020)?;
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(release.clone());
    model.held_call.store(0, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:idle-retention",
            durable_artifact_test_builder(workspace, model.clone()),
        )
        .await?;
    let mut turns = vec![agent.prompt("hold the first turn").await?];
    model.held_call_started.notified().await;
    for round in 0..16 {
        let submission = agent
            .start(CommandId::new(format!("queued:{round}"))?, "next turn")
            .await?;
        assert_eq!(
            submission.acknowledgement().disposition(),
            CommandAckDisposition::Accepted
        );
        turns.push(submission.into_turn().ok_or("turn")?);
    }
    release.notify_one();
    for turn in turns {
        let result = tokio::select! {
            biased;
            result = turn.result() => result?,
            () = store.prune_started.notified() => panic!("retention ran ahead of admitted roots"),
        };
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    store.prune_started.notified().await;
    assert_eq!(store.prune_calls.load(Ordering::Acquire), 1);
    store.block_prune.store(false, Ordering::Release);
    store.prune_release.notify_one();
    assert!(matches!(
        agent
            .prompt("after retention")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    agent.shutdown().await?;
    Ok(())
}

/// A stop that lands while the idle driver answers a control it took ahead of
/// due retention still gets that retention: whether the prune commits must
/// not depend on which of the two the driver saw first.
#[tokio::test]
async fn idle_stop_behind_a_control_still_runs_due_retention() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store
        .block_terminal_at_revision
        .store(64, Ordering::Release);
    let agent = telemetry_durability_test_agent(
        store.clone(),
        Arc::new(TelemetryEmitter::disabled()),
        workspace,
    )
    .await?;
    let terminal = loop {
        let turn = agent.prompt("reach the retention horizon").await?;
        tokio::select! {
            result = turn.result.clone().result() => {
                assert!(matches!(result?.outcome(), TurnResultOutcome::Completed(_)));
            }
            () = store.terminal_started.notified() => break turn,
        }
    };
    // The terminal commit holds the lease gate. Queue a control the idle
    // driver takes before retention, and wait for the gate behind the commit
    // so that control parks on it.
    let (reply, _answer) = tokio::sync::oneshot::channel();
    agent
        .inner
        .controls
        .try_send(crate::ActorControl::ContextStatus {
            request: crate::control::AgentContextStatusRequest {
                session_id: agent.session_id(),
                branch_id: agent.branch_id(),
                checkpoint_id: agent.checkpoint().id(),
                pending_user_input: None,
            },
            reply,
        })?;
    let mut gate = Box::pin(agent.inner.authority.pause_lease());
    assert!(futures::poll!(&mut gate).is_pending());
    store.terminal_release.notify_one();
    let paused = gate.await.ok_or("durable authority")?;
    assert!(matches!(
        terminal.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    agent.inner.shutdown.cancel();
    drop(paused);
    agent.shutdown().await?;
    assert_eq!(
        store.prune_calls.load(Ordering::Acquire),
        1,
        "the stop skipped the retention the last terminal made due"
    );
    Ok(())
}

/// A stop that lands while the terminal commit is in flight, before the
/// driver is idle again, still gets the retention that terminal makes due.
#[tokio::test]
async fn stop_during_the_terminal_commit_still_runs_due_retention() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store
        .block_terminal_at_revision
        .store(64, Ordering::Release);
    let agent = telemetry_durability_test_agent(
        store.clone(),
        Arc::new(TelemetryEmitter::disabled()),
        workspace,
    )
    .await?;
    let terminal = loop {
        let turn = agent.prompt("reach the retention horizon").await?;
        tokio::select! {
            result = turn.result.clone().result() => {
                assert!(matches!(result?.outcome(), TurnResultOutcome::Completed(_)));
            }
            () = store.terminal_started.notified() => break turn,
        }
    };
    agent.inner.shutdown.cancel();
    store.terminal_release.notify_one();
    assert!(matches!(
        terminal.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    agent.shutdown().await?;
    assert_eq!(
        store.prune_calls.load(Ordering::Acquire),
        1,
        "the stop skipped the retention the last terminal made due"
    );
    Ok(())
}

/// A stop whose queue settlement gates recovery (a queued root's cancel
/// commit fails) writes nothing more: it skips the due retention.
#[tokio::test]
async fn stop_gated_while_settling_the_queue_skips_due_retention() -> TestResult {
    // Enough history that the Working head's prune would reach the store.
    const PRE_TURNS: usize = 12;
    // The 15th queued root holds at its model call when the stop lands.
    const HELD_CALL: usize = 14;
    // Commits halt after the active turn's cancel terminal.
    const HALT_AFTER: usize = 2;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let model = artifact_test_model(0, 99_020)?;
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(release.clone());
    model.held_call.store(PRE_TURNS, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:idle-retention",
            durable_artifact_test_builder(workspace, model.clone()),
        )
        .await?;
    store.fail_prune.store(true, Ordering::Release);
    for _ in 0..PRE_TURNS {
        let result = agent.prompt("lengthen history").await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let mut turns = vec![agent.prompt("hold the first turn").await?];
    model.held_call_started.notified().await;
    for round in 0..16 {
        let submission = agent
            .start(CommandId::new(format!("queued:{round}"))?, "next turn")
            .await?;
        turns.push(submission.into_turn().ok_or("turn")?);
    }
    model
        .held_call
        .store(PRE_TURNS + HELD_CALL, Ordering::Release);
    store.fail_prune.store(false, Ordering::Release);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(30), model.held_call_started.notified())
        .await
        .map_err(|_| "no later call held")?;
    let accepted = lock_mutex(&store.accepted_commits).len();
    let prunes_before = store.prune_calls.load(Ordering::Acquire);
    store
        .halt_after_accepted_commit
        .store(accepted + HALT_AFTER, Ordering::Release);
    agent.inner.shutdown.cancel();
    release.notify_one();
    let mut outcomes = Vec::new();
    for turn in turns {
        outcomes.push(turn.result().await?.outcome().clone());
    }
    let _ = agent.shutdown().await;
    // The active turn, the first that did not complete, must settle as
    // Cancelled, so the gate came from a queued root's cancel inside
    // `cancel_queued`, not from the active turn's own terminal.
    let stopped = outcomes
        .iter()
        .position(|outcome| !matches!(outcome, TurnResultOutcome::Completed(_)))
        .ok_or("no turn was stopped")?;
    assert!(
        matches!(outcomes[stopped], TurnResultOutcome::Cancelled(_)),
        "the active turn did not settle as Cancelled"
    );
    assert!(
        outcomes[stopped + 1..]
            .iter()
            .any(|outcome| matches!(outcome, TurnResultOutcome::RecoveryRequired(_))),
        "no queued root gated recovery after the active turn"
    );
    assert_eq!(
        store.prune_calls.load(Ordering::Acquire),
        prunes_before,
        "a recovery-gated stop pruned"
    );
    Ok(())
}

/// A reopened owner starts from the horizon its predecessor's prune left in
/// the store, so the first idle terminal after a reopen does not repeat the
/// whole-history prune.
#[tokio::test]
async fn reopened_owner_does_not_repeat_its_predecessors_prune() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let model = artifact_test_model(0, 99_030)?;
    let agent = sessions
        .create(
            "create:reopened-horizon",
            durable_artifact_test_builder(Arc::clone(&workspace), model.clone()),
        )
        .await?;
    let mut round = 0;
    while store.prune_calls.load(Ordering::Acquire) == 0 {
        round += 1;
        assert!(round <= 80, "no automatic prune was attempted in 80 rounds");
        let result = agent
            .prompt(format!("round {round}"))
            .await?
            .result()
            .await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:reopened-horizon",
            address,
            durable_artifact_test_builder(workspace, model),
        )
        .await?
    else {
        return Err("a quiescent head unexpectedly required recovery".into());
    };
    let result = reopened.prompt("after reopen").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    // Retention runs before the next command, so one more turn observes it.
    let result = reopened
        .prompt("second after reopen")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        store.prune_calls.load(Ordering::Acquire),
        1,
        "the reopened owner re-ran a prune its predecessor already ran"
    );
    reopened.shutdown().await?;
    Ok(())
}
