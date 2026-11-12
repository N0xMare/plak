use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// A probe that starts and then waits until the turn cancels it.
struct ParkingProbe {
    started: AtomicBool,
}

impl plakous_tools::ToolExecutor for ParkingProbe {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            context.cancellation().cancelled().await;
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "probe joined cancellation",
            ))
        })
    }
}

fn parking_probe_registry(tool: &Arc<ParkingProbe>) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Park until the turn is cancelled.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let probe = Arc::clone(tool);
    let executor: Arc<dyn plakous_tools::ToolExecutor> = probe;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        executor,
    ))?;
    Ok(registry.freeze())
}

fn history_text(result: &TurnResult) -> String {
    result
        .conversation()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal_records(
    result: &TurnResult,
) -> Vec<crate::runtime::terminal::DurableTerminalResultRecord> {
    result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter_map(|record| {
            let DurableOperationState::Finalized(evidence) = record.state() else {
                return None;
            };
            read_terminal_result_record(evidence.bytes()).ok()
        })
        .collect()
}

/// Restores a provider-programmatic tool batch whose probe has started and
/// not been observed, then abandons or cancels it.
async fn restored_parking_batch(end_with_abandonment: bool) -> TestResult<Arc<TurnResult>> {
    restored_batch_ended(end_with_abandonment, true, false).await
}

/// Restores the same batch and ends it with an abandon (or a plain cancel)
/// either once its probe started or, without `wait_for_probe`, as soon as
/// the open returns: what a host that marks a lost child does.
async fn restored_batch_ended(
    end_with_abandonment: bool,
    wait_for_probe: bool,
    hold_before_start: bool,
) -> TestResult<Arc<TurnResult>> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_180,
            false,
        )
        .with_tool_first()
        .with_programmatic_local_tool()?,
    );
    let probe = Arc::new(ParkingProbe {
        started: AtomicBool::new(false),
    });
    let registry = parking_probe_registry(&probe)?;
    let builder = |registry: FrozenToolRegistry| -> TestResult<AgentBuilder> {
        Ok(
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry)?
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .with_scheduled_compaction_enabled(false),
        )
    };
    let store = Arc::new(TestCreateStore::default());
    store.block_after_hosted_effect_count(0);
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let original = sessions
        .create("create:restored-batch-abandon", builder(registry.clone())?)
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
        .open("open:restored-batch-abandon", address, builder(registry)?)
        .await?
    else {
        return Err("hosted tool-batch frame did not restore as working".into());
    };
    if hold_before_start {
        let mut hold = agent.session_endpoint().hold_step_boundary()?;
        while !matches!(hold.progress()?, crate::control::boundary::BoundaryProgress::Held {turn:Some(id),..} if id==turn.turn_id())
        {
            hold.changed().await?;
        }
        assert!(
            !probe.started.load(Ordering::Acquire),
            "restored undispatched slot crossed a held boundary"
        );
        hold.release().await?;
    }
    if wait_for_probe {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !probe.started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "restored probe did not start")?;
    }
    if end_with_abandonment {
        turn.control()
            .abandon(crate::AbandonmentCause::ProcessLost)
            .await?;
    } else {
        turn.control().cancel().await?;
    }
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result().result()).await??;
    assert_eq!(model.calls.load(Ordering::Acquire), 1, "no provider replay");
    agent.shutdown().await?;
    let _ = original.shutdown().await;
    Ok(result)
}

fn assert_one_finalized_baton(result: &TurnResult) {
    assert!(
        matches!(
            result.checkpoint().manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ),
        "abandoned hosted batch left {:?}",
        result.checkpoint().manifest().lifecycle()
    );
    let continuations = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::ToolBatchContinuation { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(continuations.len(), 1, "one hosted baton");
    assert!(
        matches!(
            continuations[0].state(),
            DurableOperationState::Finalized(_)
        ),
        "the baton stays open: {:?}",
        continuations[0].state()
    );
    let terminals = terminal_records(result);
    assert_eq!(terminals.len(), 1, "exactly one terminal record");
}

#[tokio::test]
async fn restored_tool_batch_abandoned_as_process_lost_writes_the_abandonment_note() -> TestResult {
    let result = restored_parking_batch(true).await?;
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    let text = history_text(result.as_ref());
    assert!(
        !text.contains("the user cancelled"),
        "an owner loss must not say the operator cancelled: {text}"
    );
    assert!(
        text.contains("while a tool was"),
        "a started unobserved probe is a running tool: {text}"
    );
    assert_one_finalized_baton(result.as_ref());
    let terminal = &terminal_records(result.as_ref())[0];
    let report = terminal
        .abandonment
        .as_ref()
        .ok_or("cancelled terminal omitted its abandonment report")?;
    assert_eq!(report.cause(), crate::AbandonmentCause::ProcessLost);
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::RunningTool);
    Ok(())
}

#[tokio::test]
async fn restored_tool_batch_plain_cancel_keeps_the_operator_note_and_null_report() -> TestResult {
    let result = restored_parking_batch(false).await?;
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    let text = history_text(result.as_ref());
    assert!(
        text.contains("the user cancelled the request above"),
        "a plain cancel keeps the operator note: {text}"
    );
    assert_one_finalized_baton(result.as_ref());
    let terminal = &terminal_records(result.as_ref())[0];
    assert!(
        terminal.abandonment.is_none(),
        "a plain cancel must keep a null abandonment report"
    );
    Ok(())
}

/// An abandon that reaches the restored turn before its batch starts, as a
/// host marking a lost child sends it right after the open, closes the turn
/// with one terminal and the abandonment note, as a plain cancel does.
#[tokio::test]
async fn a_restored_tool_batch_abandoned_before_it_starts_closes_with_one_terminal() -> TestResult {
    for abandon in [true, false] {
        let result = restored_batch_ended(abandon, false, false).await?;
        let TurnResultOutcome::Cancelled(_) = result.outcome() else {
            let detail = match result.outcome() {
                TurnResultOutcome::RecoveryRequired(error) | TurnResultOutcome::Failed(error) => {
                    error.message().to_owned()
                }
                _ => String::new(),
            };
            return Err(format!(
                "abandon={abandon}: expected a cancelled turn, got {:?}: {detail}",
                result.outcome()
            )
            .into());
        };
        assert_one_finalized_baton(result.as_ref());
        let text = history_text(result.as_ref());
        let terminal = &terminal_records(result.as_ref())[0];
        if abandon {
            assert!(!text.contains("the user cancelled"), "{text}");
            let report = terminal
                .abandonment
                .as_ref()
                .ok_or("the cancelled terminal omitted its abandonment report")?;
            assert_eq!(report.cause(), crate::AbandonmentCause::ProcessLost);
        } else {
            assert!(
                text.contains("the user cancelled the request above"),
                "{text}"
            );
            assert!(terminal.abandonment.is_none());
        }
    }
    Ok(())
}

/// The head a hosted tool batch leaves when its process is cut after
/// `completed` of its effects completed, with the batch's baton.
async fn hosted_batch_cut(
    completed: usize,
    two_calls: bool,
) -> TestResult<(RuntimeCheckpoint, TurnId, OperationId)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        1,
        if two_calls { 9_385 } else { 9_380 },
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
    store.block_after_hosted_effect_count(completed);
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let original = sessions
        .create(
            "create:hosted-batch-cut",
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry)?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
        .await?;
    let turn = original.prompt("cut the hosted tool batch").await?;
    let turn_id = turn.id();
    let checkpoint = loop {
        if store.commits_blocked.load(Ordering::Acquire) {
            break original.checkpoint().as_ref().clone();
        }
        tokio::task::yield_now().await;
    };
    original.abort_driver_for_test();
    let baton = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::ToolBatchContinuation { .. }
            ) && matches!(
                record.state(),
                DurableOperationState::ContinuationRequired(_)
            )
        })
        .map(|record| OperationId(record.operation_id()))
        .ok_or("the cut has no open tool-batch baton")?;
    Ok((checkpoint, turn_id, baton))
}

/// Restored slots do not run while the driver is held; release still joins cancellation.
#[tokio::test]
async fn boundary_hold_restored_batch_parks_before_its_first_undispatched_slot() -> TestResult {
    let result = restored_batch_ended(false, true, true).await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    assert_one_finalized_baton(result.as_ref());
    assert_eq!(terminal_records(result.as_ref()).len(), 1);
    Ok(())
}

/// A restored batch whose slots never started: nothing ran.
#[tokio::test]
async fn a_live_report_names_a_batch_whose_slots_never_started_nothing_started() -> TestResult {
    let (checkpoint, turn_id, baton) = hosted_batch_cut(0, false).await?;
    let (report, note) = crate::runtime::durable::turn_recovery::live_cancel_report(
        &checkpoint,
        turn_id,
        crate::AbandonmentCause::ProcessLost,
        Some(baton),
    )?;
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::NothingStarted);
    assert_eq!(report.cause(), crate::AbandonmentCause::ProcessLost);
    assert!(note.contains("never started"), "{note}");
    Ok(())
}

/// A restored batch whose first effect completed and whose result was never
/// added gets the recovery closure's "tool finished" note, not the running
/// tool's.
#[tokio::test]
async fn a_live_report_names_a_completed_unadded_effect_with_the_finished_tool_note() -> TestResult
{
    let (checkpoint, turn_id, baton) = hosted_batch_cut(1, true).await?;
    let (report, note) = crate::runtime::durable::turn_recovery::live_cancel_report(
        &checkpoint,
        turn_id,
        crate::AbandonmentCause::Interrupted,
        Some(baton),
    )?;
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::RunningTool);
    assert_eq!(report.cause(), crate::AbandonmentCause::Interrupted);
    assert!(
        note.contains("after a tool finished, but its result was not added"),
        "{note}"
    );
    Ok(())
}

/// A completed tool round with nothing after it is the tool-round claim and
/// note, as the recovery closure reads the same head.
#[tokio::test]
async fn a_live_report_names_a_completed_tool_round() -> TestResult {
    let cut = landed_round_abandonment::completed_tool_round_cut(
        SessionUpdateFixtureMode::JoinedForeground,
        false,
        false,
    )
    .await?;
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        ..
    } = cut.manifest().lifecycle()
    else {
        return Err("a working turn".into());
    };
    let (report, note) = crate::runtime::durable::turn_recovery::live_cancel_report(
        &cut,
        *turn_id,
        crate::AbandonmentCause::ProcessLost,
        None,
    )?;
    assert_eq!(
        report.claim(),
        crate::AbandonedTurnClaim::ToolRoundCompleted
    );
    assert!(report.tool_round_completed());
    assert!(note.contains("after its tool round completed"), "{note}");
    Ok(())
}

/// An open model request, live, is the model-request claim.
#[tokio::test]
async fn an_abandoned_open_model_request_reports_a_model_request() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_390,
            false,
        )
        .with_block_call_after(0),
    );
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:abandon-model-request",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let turn = agent.prompt("wait on the model").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "the model request never started")?;
    turn.control()
        .abandon(crate::AbandonmentCause::Interrupted)
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    let text = history_text(&result);
    assert!(!text.contains("the user cancelled"), "{text}");
    let records = terminal_records(&result);
    let report = records
        .iter()
        .find_map(|record| record.abandonment.as_ref())
        .ok_or("the cancelled terminal omitted its report")?;
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::ModelRequest);
    assert_eq!(report.cause(), crate::AbandonmentCause::Interrupted);
    agent.shutdown().await?;
    Ok(())
}

/// An abandon of a queued root that never started reports nothing started,
/// on its queued-cancellation terminal.
#[tokio::test]
async fn an_abandoned_queued_root_reports_nothing_started() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_395,
            false,
        )
        .with_block_call_after(0),
    );
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:abandon-queued-root",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let first = agent.prompt("hold the model").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "the first model request never started")?;
    let queued = agent.prompt("queued behind it").await?;
    let queued_id = queued.id();
    queued
        .control()
        .abandon(crate::AbandonmentCause::ProcessLost)
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(30), queued.result()).await??;
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    let record = terminal_records(&result)
        .into_iter()
        .find(|record| record.turn_id == queued_id)
        .ok_or("the queued root has no terminal record")?;
    let report = record
        .abandonment
        .ok_or("the queued cancellation omitted its report")?;
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::NothingStarted);
    assert_eq!(report.cause(), crate::AbandonmentCause::ProcessLost);
    first.control().cancel().await?;
    let _ = tokio::time::timeout(Duration::from_secs(30), first.result()).await?;
    agent.shutdown().await?;
    Ok(())
}

/// An abandon of the running turn while a second turn is queued: the
/// queued root's prepared command is not the running turn's own, so the
/// closure fold admits no row for it and the report cannot be built. The
/// turn is cancelled anyway, as a plain cancel ends it, and the abandon
/// reports the failure typed.
#[tokio::test]
async fn an_abandon_whose_report_cannot_be_built_still_cancels() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_397,
            false,
        )
        .with_block_call_after(0),
    );
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:abandon-report-failure",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let first = agent.prompt("hold the model").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "the first model request never started")?;
    let queued = agent.prompt("queued behind it").await?;
    let control = first.control();
    let abandoned = control.abandon(crate::AbandonmentCause::ProcessLost).await;
    let result = tokio::time::timeout(Duration::from_secs(30), first.result()).await??;
    let report = terminal_records(&result)
        .into_iter()
        .find(|record| record.turn_id == control.turn_id)
        .and_then(|record| record.abandonment);
    let text = history_text(&result);
    eprintln!(
        "F5 abandon={abandoned:?} outcome={:?} report={report:?} note_user={}",
        result.outcome(),
        text.contains("the user cancelled")
    );
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    let error = abandoned
        .err()
        .ok_or("the report failure was not reported")?;
    assert_eq!(error.kind(), ControlErrorKind::Internal);
    assert!(report.is_none(), "a failed report leaves a plain cancel");
    assert!(text.contains("the user cancelled"), "{text}");
    queued.control().cancel().await?;
    let _ = tokio::time::timeout(Duration::from_secs(30), queued.result()).await?;
    agent.shutdown().await?;
    Ok(())
}

/// An abandon that reaches a turn its operator already cancelled finds that
/// cancel requested and records nothing: the terminal keeps the operator's
/// note and a null report.
#[tokio::test]
async fn an_abandon_after_an_operator_cancel_keeps_the_operator_note() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_396,
            false,
        )
        .with_tool_first(),
    );
    let probe = Arc::new(ParkingProbe {
        started: AtomicBool::new(false),
    });
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:abandon-after-cancel",
            durable_decision_builder(workspace, model, parking_probe_registry(&probe)?)?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
        .await?;
    let turn = agent.prompt("run the probe").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !probe.started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "the probe never started")?;
    // Both reach the driver before it settles the cancel: the abandon finds
    // it requested.
    let control = turn.control();
    let (cancelled, abandoned) = tokio::join!(
        control.cancel(),
        control.abandon(crate::AbandonmentCause::ProcessLost)
    );
    assert_eq!(cancelled?, CancellationStatus::Requested);
    assert_eq!(abandoned?, CancellationStatus::AlreadyRequested);
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    let text = history_text(&result);
    assert!(text.contains("the user cancelled"), "{text}");
    assert!(
        terminal_records(&result)
            .iter()
            .all(|record| record.abandonment.is_none()),
        "the operator's cancel keeps a null report"
    );
    agent.shutdown().await?;
    Ok(())
}

/// Two concurrent abandons of one turn: the first stores its cause, and its
/// cancel is the one that stops the turn, so the report carries that cause
/// and history the abandonment note. The second finds the cancel requested
/// and its cause is dropped.
#[tokio::test]
async fn the_first_of_two_concurrent_abandons_keeps_its_cause() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_394,
            false,
        )
        .with_tool_first(),
    );
    let probe = Arc::new(ParkingProbe {
        started: AtomicBool::new(false),
    });
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:two-abandons",
            durable_decision_builder(workspace, model, parking_probe_registry(&probe)?)?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
        .await?;
    let turn = agent.prompt("run the probe").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !probe.started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "the probe never started")?;
    let control = turn.control();
    let (first, second) = tokio::join!(
        control.abandon(crate::AbandonmentCause::ProcessLost),
        control.abandon(crate::AbandonmentCause::Interrupted)
    );
    assert_eq!(first?, CancellationStatus::Requested);
    assert_eq!(second?, CancellationStatus::AlreadyRequested);
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    let text = history_text(&result);
    assert!(!text.contains("the user cancelled"), "{text}");
    let report = terminal_records(&result)
        .into_iter()
        .find_map(|record| record.abandonment)
        .ok_or("the stopping cancel omitted its report")?;
    assert_eq!(report.cause(), crate::AbandonmentCause::ProcessLost);
    agent.shutdown().await?;
    Ok(())
}

/// A turn that carries a cause and a recorded report but then completes
/// normally ends `Completed` with no report.
#[tokio::test]
async fn a_cause_then_normal_completion_ends_completed_without_a_report() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_398,
            false,
        )
        .with_ttft_probe(Duration::from_millis(200)),
    );
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:abandon-after-completion",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let turn = agent.prompt("answer").await?;
    let control = turn.control();
    control.shared.set_abandonment_cause(
        CommandId::new("cancel:never-submitted")?,
        crate::AbandonmentCause::ProcessLost,
    );
    control.shared.record_abandonment(
        crate::AbandonedTurnReport {
            model_round_completed: false,
            tool_round_completed: false,
            operations: Vec::new(),
            effects: Vec::new(),
            uncommitted_tool_calls: Vec::new(),
            claim: crate::AbandonedTurnClaim::NothingStarted,
            cause: crate::AbandonmentCause::ProcessLost,
        },
        "abandoned".to_owned(),
    );
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert!(
        terminal_records(&result)
            .iter()
            .all(|record| record.abandonment.is_none()),
        "a completed turn carries no abandonment report"
    );
    agent.shutdown().await?;
    Ok(())
}

/// How many transitions the head's tool-batch baton has, if it has one.
fn baton_transitions(checkpoint: &RuntimeCheckpoint) -> Option<usize> {
    checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::ToolBatchContinuation { .. }
            )
        })
        .map(|record| record.transitions().len())
}

/// A restored turn that ended leaves the driver naming no baton: a later
/// live turn's cancel finalizes nothing of the restored frame's and settles
/// as any cancel does.
#[tokio::test]
async fn a_live_cancel_after_a_restored_turn_leaves_the_restored_baton_alone() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_399,
            false,
        )
        .with_tool_first()
        .with_programmatic_local_tool()?
        .with_block_call_after(2),
    );
    let registry = one_mutating_tool_registry()?;
    let store = Arc::new(TestCreateStore::default());
    store.block_after_promoted_hosted();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let builder = |registry: FrozenToolRegistry| -> TestResult<AgentBuilder> {
        Ok(
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry)?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
        )
    };
    let original = sessions
        .create("create:restored-then-live", builder(registry.clone())?)
        .await?;
    let _turn = original.prompt("restore then cancel later").await?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire)
            && matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                    ..
                }
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
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open("open:restored-then-live", address, builder(registry)?)
        .await?
    else {
        return Err("the hosted tool batch did not restore".into());
    };
    let restored = tokio::time::timeout(Duration::from_secs(30), turn.result().result()).await??;
    assert!(matches!(
        restored.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let before = baton_transitions(restored.checkpoint());
    let live = agent.prompt("a later live turn").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while model.calls.load(Ordering::Acquire) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "the live model request never started")?;
    live.control().cancel().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), live.result()).await??;
    let TurnResultOutcome::Cancelled(_) = result.outcome() else {
        return Err(format!("expected a cancelled turn, got {:?}", result.outcome()).into());
    };
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(
        baton_transitions(result.checkpoint()),
        before,
        "the restored baton is untouched"
    );
    agent.shutdown().await?;
    let _ = original.shutdown().await;
    Ok(())
}
