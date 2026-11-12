//! The host hold cannot dispatch work, invent terminals, or outlive its owner.
use super::*;
use crate::ActorControl;
use crate::control::boundary::{BoundaryError, BoundaryHold, BoundaryPhase, BoundaryProgress};

fn fixture(entry_base: u128) -> TestResult<(tempfile::TempDir, Agent, Arc<ArtifactModel>)> {
    let directory = tempfile::tempdir()?;
    let (builder, model) = placeholder_fixture(
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        entry_base,
        400_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .config(AgentConfig::default().with_model_operation_timeout(Duration::from_secs(1)))
        .build()?;
    Ok((directory, agent, model))
}

async fn held(hold: &mut BoundaryHold, turn: Option<TurnId>) -> TestResult {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(hold.progress()?, BoundaryProgress::Held { turn: observed, .. } if observed == turn) {
                return Ok(());
            }
            hold.changed().await?;
        }
    }).await?
}

#[tokio::test(start_paused = true)]
async fn boundary_hold_idle_then_internal_admission_excludes_generation_clocks() -> TestResult {
    let (_directory, agent, model) = fixture(390_000)?;
    let endpoint = agent.session_endpoint();
    let mut hold = endpoint.hold_step_boundary()?;
    held(&mut hold, None).await?;
    let command = CommandId::new("boundary:internal-admission")?;
    let submission = agent.start(command.clone(), "internal wake").await?;
    let turn = submission.into_turn().ok_or("admitted turn")?;
    held(&mut hold, Some(turn.id())).await?;
    tokio::time::advance(Duration::from_secs(20)).await;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert!(matches!(
        hold.progress()?,
        BoundaryProgress::Held { turn: Some(_), .. }
    ));
    hold.release().await?;
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.model_rounds(), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    assert_exact_terminal_result(&result, &command)?;
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn boundary_hold_second_request_refuses_and_old_drop_cannot_release_new_generation()
-> TestResult {
    let (_directory, agent, model) = fixture(391_000)?;
    let endpoint = agent.session_endpoint();
    let mut first = endpoint.hold_step_boundary()?;
    held(&mut first, None).await?;
    assert!(matches!(
        endpoint.hold_step_boundary(),
        Err(BoundaryError::Busy)
    ));
    let mut releasing = Box::pin(first.release());
    assert!(futures_util::poll!(releasing.as_mut()).is_pending());
    let mut second = endpoint.hold_step_boundary()?;
    drop(releasing);
    held(&mut second, None).await?;
    let turn = agent.prompt("still held").await?;
    held(&mut second, Some(turn.id())).await?;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    drop(second);
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn boundary_hold_dropped_before_owner_poll_cannot_pause_a_resumed_driver() -> TestResult {
    let (_directory, agent, model) = fixture(392_000)?;
    let checkpoint = agent.checkpoint();
    while agent
        .inner
        .controls
        .try_send(ActorControl::ContextStatus {
            request: crate::AgentContextStatusRequest {
                session_id: agent.session_id(),
                branch_id: agent.branch_id(),
                checkpoint_id: checkpoint.id(),
                pending_user_input: None,
            },
            reply: tokio::sync::oneshot::channel().0,
        })
        .is_ok()
    {}
    assert_eq!(agent.inner.controls.capacity(), 0);
    let endpoint = agent.session_endpoint();
    let stale = endpoint.hold_step_boundary()?;
    drop(stale);
    while agent.inner.controls.capacity() == 0 {
        tokio::task::yield_now().await;
    }
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        agent
            .prompt("do not apply a late request")
            .await?
            .result()
            .await
    })
    .await??;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn boundary_hold_never_interrupts_paid_compaction_or_starts_another_fold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        workspace.clone(),
        "create:boundary-paid",
        396_000,
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
    let request = AgentCompactSessionRequest::new(
        "compact:boundary-paid".to_owned(),
        source.session_id,
        source.branch_id,
        source.checkpoint_id,
        AgentCompactionReason::Manual,
        builder,
    );
    let endpoint = agent.session_endpoint();
    let compaction = tokio::spawn(async move { endpoint.compact_session(request).await });
    while model.starts.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }
    let endpoint = agent.session_endpoint();
    let mut hold = endpoint.hold_step_boundary()?;
    assert_eq!(
        hold.progress()?,
        BoundaryProgress::Draining {
            turn: None,
            phase: BoundaryPhase::Compaction
        }
    );
    tokio::time::advance(Duration::from_millis(100)).await;
    assert!(!compaction.is_finished());
    assert!(matches!(
        hold.progress()?,
        BoundaryProgress::Draining {
            phase: BoundaryPhase::Compaction,
            ..
        }
    ));
    tokio::time::advance(Duration::from_secs(2)).await;
    let outcome = compaction
        .await?
        .map_err(|error| format!("compaction: {error:?}"))?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::Compacted { .. }
    ));
    held(&mut hold, None).await?;
    let refused = endpoint
        .compact_session(AgentCompactSessionRequest {
            operation_id: "compact:boundary-refused".into(),
            session_id: agent.session_id(),
            branch_id: agent.branch_id(),
            checkpoint_id: agent.checkpoint().id(),
            reason: AgentCompactionReason::Manual,
            pending_user_input: None,
            runtime: None,
            fallback_of: None,
        })
        .await;
    assert!(matches!(refused, Err(AgentSessionEndpointError::Busy)));
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    hold.release().await?;
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn boundary_hold_owner_shutdown_wakes_waiters_without_reopening() -> TestResult {
    let (_directory, agent, _model) = fixture(393_000)?;
    let endpoint = agent.session_endpoint();
    let mut hold = endpoint.hold_step_boundary()?;
    held(&mut hold, None).await?;
    agent.shutdown().await?;
    assert_eq!(hold.changed().await, Err(BoundaryError::Unavailable));
    assert!(matches!(
        endpoint.hold_step_boundary(),
        Err(BoundaryError::Unavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn boundary_hold_cancelled_turn_has_one_terminal_and_the_next_turn_stays_held() -> TestResult
{
    let (_directory, agent, model) = fixture(397_000)?;
    let mut hold = agent.session_endpoint().hold_step_boundary()?;
    held(&mut hold, None).await?;
    let command = CommandId::new("boundary:cancel")?;
    let turn = agent
        .start(command.clone(), "cancel before dispatch")
        .await?
        .into_turn()
        .ok_or("turn")?;
    held(&mut hold, Some(turn.id())).await?;
    turn.control().cancel().await?;
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    assert_exact_terminal_result(&result, &command)?;
    held(&mut hold, None).await?;
    let turn = agent
        .prompt("the hold belongs to the driver, not the cancelled turn")
        .await?;
    held(&mut hold, Some(turn.id())).await?;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    hold.release().await?;
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn boundary_hold_does_not_disable_the_next_model_operation_deadline() -> TestResult {
    let (_directory, agent, model) = fixture(398_000)?;
    model.held_call.store(0, Ordering::Release);
    *lock_mutex(&model.held_call_release) = Some(Arc::new(Notify::new()));
    let mut hold = agent.session_endpoint().hold_step_boundary()?;
    held(&mut hold, None).await?;
    let turn = agent.prompt("the next generation still times out").await?;
    held(&mut hold, Some(turn.id())).await?;
    tokio::time::advance(Duration::from_secs(20)).await;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    hold.release().await?;
    model.held_call_started.notified().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(error) if error.model_kind() == Some(ModelErrorKind::Timeout)),
        "{:?}",
        result.outcome()
    );
    assert!(model.held_call_joined.load(Ordering::Acquire));
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn boundary_hold_waits_for_model_and_parks_before_tool_dispatch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            394_000,
            false,
        )
        .with_tool_first(),
    );
    model.held_call.store(0, Ordering::Release);
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(release.clone());
    let tool = Arc::new(cancelled_steer::HeldTool::default());
    let registry = cancelled_steer::held_probe_registry(tool.clone())?;
    let runtime = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create(
            "create:boundary-model",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model.clone(),
                registry,
            )?,
        )
        .await?;
    let turn = agent.prompt("one tool then reply").await?;
    model.held_call_started.notified().await;
    let mut hold = agent.session_endpoint().hold_step_boundary()?;
    assert_eq!(
        hold.progress()?,
        BoundaryProgress::Draining {
            turn: Some(turn.id()),
            phase: BoundaryPhase::Model
        }
    );
    release.notify_one();
    held(&mut hold, Some(turn.id())).await?;
    assert!(!tool.started.load(Ordering::Acquire));
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    tool.release.store(true, Ordering::Release);
    hold.release().await?;
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn boundary_hold_effect_must_join_before_held() -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            395_000,
            false,
        )
        .with_tool_first(),
    );
    let tool = Arc::new(cancelled_steer::HeldTool::default());
    let registry = cancelled_steer::held_probe_registry(tool.clone())?;
    let runtime = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create(
            "create:boundary-effect",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model.clone(),
                registry,
            )?,
        )
        .await?;
    let turn = agent.prompt("finish the effect").await?;
    wait_for_scheduler_fact(&tool.started).await;
    let mut hold = agent.session_endpoint().hold_step_boundary()?;
    assert_eq!(
        hold.progress()?,
        BoundaryProgress::Draining {
            turn: Some(turn.id()),
            phase: BoundaryPhase::Effect
        }
    );
    tool.release.store(true, Ordering::Release);
    held(&mut hold, Some(turn.id())).await?;
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    hold.release().await?;
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    Ok(())
}
