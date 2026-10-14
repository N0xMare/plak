use super::*;
use plakous_model::{StableRequestPrefixDrift, StableRequestPrefixField};

/// The operation-budget wrap-up withdraws tool selection, so its stable prefix
/// differs from the round that produced the live family artifact; the next
/// turn, back on the live prefix, differs from the wrap-up's artifact the same
/// way. Both rounds proceed and name only `tool-choice`.
#[tokio::test]
async fn wrap_up_round_names_its_artifact_prefix_drift_and_completes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            3,
            9_925,
            false,
        )
        .with_tool_call_on(0),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
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
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .telemetry(Arc::new(TelemetryEmitter::enabled(sink.clone())))
        .config(AgentConfig::default().with_max_model_rounds(1))
        .build()?;
    let drifts = || {
        lock_mutex(&sink.observations)
            .iter()
            .filter_map(|observation| match observation.kind() {
                TelemetryObservationKind::ModelArtifactPrefixDrift { fields } => Some(fields),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let tool_choice =
        StableRequestPrefixDrift::default().with(StableRequestPrefixField::ToolChoice);

    let result = agent.prompt("probe, then wrap up").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(drifts(), vec![tool_choice]);

    let result = agent.prompt("next turn").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(model.calls.load(Ordering::Acquire), 3);
    assert_eq!(drifts(), vec![tool_choice, tool_choice]);
    // Every later round still offered the route its live artifact; dropping
    // it is the route's decision, made on the binding alone.
    assert!(lock_mutex(&model.observed)[1..].iter().all(Option::is_some));
    agent.shutdown().await?;
    Ok(())
}
