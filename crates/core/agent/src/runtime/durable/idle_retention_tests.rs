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
