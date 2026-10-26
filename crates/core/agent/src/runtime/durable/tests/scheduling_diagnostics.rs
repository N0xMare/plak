use super::*;

pub(super) struct ArtifactOperationScope;
impl plakous_model::ModelOperationScope for ArtifactOperationScope {}

#[tokio::test]
async fn exhausted_threshold_does_not_suppress_one_provider_overflow_ladder() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        901_000,
        40_000,
        &[100],
        &[2],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder.build()?;
    for prompt in ["older requirement", "latest requirement"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    model
        .summary_context_overhead
        .store(1_000_000, Ordering::Release);
    model
        .tool_successor_capacity_loss
        .store(36_000, Ordering::Release);
    // The unchanged next round reports overflow after the first free ladder
    // found no smaller legal successor. Provider overflow still gets its one
    // distinct recovery ladder, even on that unchanged source.
    model.overflow_once_at_call.store(3, Ordering::Release);
    let mut turn = agent.prompt("read one small tool result").await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(error) if error.model_kind() == Some(ModelErrorKind::ContextLimit)),
        "{:?}",
        result.outcome()
    );
    let mut starts = 0;
    while let Ok(event) = events.recv().await {
        match event.kind() {
            AgentEventKind::CompactionStarted { .. } => starts += 1,
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    assert_eq!(
        starts, 2,
        "one threshold ladder and one provider-overflow recovery"
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn optional_compaction_measurement_refusals_are_typed_and_nonfatal() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        348_000,
        40_000,
        &[100],
        &[1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.item_context.store(true, Ordering::Release);
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let agent = builder
        .telemetry(Arc::new(TelemetryEmitter::enabled(sink.clone())))
        .build()?;
    assert!(matches!(
        agent
            .prompt("seed history")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    model
        .refuse_scheduling_measurement
        .store(true, Ordering::Release);
    let turn = agent.prompt("read the small result").await?;
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    let refusals = lock_mutex(&sink.observations)
        .iter()
        .filter(|observation| {
            matches!(
                observation.kind(),
                TelemetryObservationKind::CompactionFinished {
                    outcome: CompactionEventOutcome::Unavailable,
                    refusal: Some(crate::CompactionRefusalReason::Unavailable),
                    elapsed: Duration::ZERO,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        refusals, 2,
        "before-prompt and tool-group scheduling probes"
    );
    agent.shutdown().await?;
    Ok(())
}
