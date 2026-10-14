use super::*;

pub(super) struct ArtifactOperationScope;
impl plakous_model::ModelOperationScope for ArtifactOperationScope {}

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
