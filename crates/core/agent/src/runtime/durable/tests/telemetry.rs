use super::*;

pub(super) struct RecordingTelemetrySink {
    pub(super) observations: Mutex<Vec<TelemetryObservation>>,
}

impl TelemetrySink for RecordingTelemetrySink {
    fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
        lock_mutex(&self.observations).push(observation);
        Ok(())
    }
}

#[tokio::test]
async fn runtime_telemetry_records_turn_model_tool_compaction_and_durability() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_900,
            false,
        )
        .with_tool_call_on(2),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = sessions
        .create(
            "create:telemetry-lifecycle",
            AgentBuilder::new(configured, registry, Arc::clone(&workspace))
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                )
                .telemetry(telemetry),
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let result = agent.prompt("use the probe").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 1);
    {
        let observations = lock_mutex(&sink.observations);
        assert!(
            observations.iter().any(|observation| matches!(
                observation.kind(),
                TelemetryObservationKind::TurnStarted { .. }
            )),
            "missing TurnStarted"
        );
        let turn_finished = observations.iter().find_map(|observation| {
            let TelemetryObservationKind::TurnFinished {
                model_wall,
                model_attempts,
                tool_calls,
                ..
            } = observation.kind()
            else {
                return None;
            };
            (tool_calls == 1).then_some((model_wall, model_attempts, tool_calls))
        });
        let (model_wall, model_attempts, tool_calls) =
            turn_finished.ok_or("missing TurnFinished")?;
        assert!(model_wall > Duration::ZERO);
        assert_eq!(model_attempts, 2);
        assert_eq!(tool_calls, 1);
        assert!(
            observations.iter().any(|observation| {
                matches!(observation.kind(), TelemetryObservationKind::ModelStarted)
                    && observation.context().attempt_id().is_some()
            }),
            "missing ModelStarted with attempt id"
        );
        assert!(
            observations.iter().any(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::ModelFirstEvent { .. }
                ) && observation.context().attempt_id().is_some()
            }),
            "missing ModelFirstEvent with attempt id"
        );
        assert!(
            observations.iter().any(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::ModelFinished { .. }
                ) && observation.context().attempt_id().is_some()
            }),
            "missing ModelFinished with attempt id"
        );
        assert!(
            observations.iter().any(|observation| {
                matches!(observation.kind(), TelemetryObservationKind::ToolStarted)
                    && observation.context().call_id().is_some()
            }),
            "missing ToolStarted with call id"
        );
        assert!(
            observations.iter().any(|observation| {
                let TelemetryObservationKind::ToolFinished { output_bytes, .. } =
                    observation.kind()
                else {
                    return false;
                };
                observation.context().call_id().is_some() && output_bytes > 0
            }),
            "missing ToolFinished with call id and output bytes"
        );
        assert!(
            observations.iter().any(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::DurabilityAttemptFinished {
                        attempt: DurabilityAttemptKind::InitialCommit,
                        outcome: DurabilityAttemptOutcome::Committed,
                        ..
                    }
                )
            }),
            "missing committed DurabilityAttemptFinished"
        );
    }
    agent.shutdown().await?;
    Ok(())
}

/// A failed automatic prune after terminal publication is only a telemetry
/// observation: the published result and the branch head stay as they were,
/// and later turns run normally.
#[tokio::test]
async fn a_failed_automatic_prune_emits_retention_failed_and_changes_nothing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store.fail_prune.store(true, Ordering::Release);
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let agent = telemetry_durability_test_agent(Arc::clone(&store), telemetry, workspace).await?;
    let mut round = 0;
    while store.prune_calls.load(Ordering::Acquire) == 0 {
        round += 1;
        assert!(round <= 80, "no automatic prune was attempted in 80 rounds");
        let head = agent.checkpoint_id();
        let result = agent
            .prompt(format!("round {round}"))
            .await?
            .result()
            .await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        assert_ne!(agent.checkpoint_id(), head);
        if store.prune_calls.load(Ordering::Acquire) > 0 {
            assert_eq!(
                agent.checkpoint_id(),
                result.checkpoint_id(),
                "a failed prune moved the head past the published result"
            );
        }
    }
    // The driver runs retention before its next command, so the observation
    // is recorded once the following turn has been admitted and finished.
    let result = agent.prompt("after the failure").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let failed = lock_mutex(&sink.observations)
        .iter()
        .filter(|observation| {
            matches!(
                observation.kind(),
                TelemetryObservationKind::RetentionFinished {
                    status: TelemetryTerminalStatus::Failed,
                    pruned_checkpoints: 0,

                    ..
                }
            )
        })
        .count();
    assert!(failed >= 1, "no failed RetentionFinished was observed");
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn driver_cleanup_emits_each_phase_timing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let agent = telemetry_durability_test_agent(store, telemetry, workspace).await?;
    agent.shutdown().await?;
    let phases: Vec<_> = lock_mutex(&sink.observations)
        .iter()
        .filter_map(|observation| match observation.kind() {
            TelemetryObservationKind::DurabilityAttemptFinished { attempt, .. } => {
                Some(format!("{attempt:?}"))
            }
            _ => None,
        })
        .collect();
    for phase in [
        "CleanupToolDrain",
        "CleanupJobTerminals",
        "CleanupHeartbeatJoin",
        "CleanupLeaseRelease",
        "CleanupFlush",
    ] {
        assert!(
            phases.iter().any(|observed| observed == phase),
            "missing {phase}: {phases:?}"
        );
    }
    Ok(())
}

fn durability_telemetry_groups(
    observations: &[TelemetryObservation],
) -> Vec<(
    Vec<(DurabilityAttemptKind, DurabilityAttemptOutcome)>,
    TelemetryTerminalStatus,
)> {
    let mut groups = Vec::new();
    let mut attempts = Vec::new();
    for observation in observations {
        match observation.kind() {
            TelemetryObservationKind::DurabilityAttemptFinished {
                attempt, outcome, ..
            } => attempts.push((attempt, outcome)),
            TelemetryObservationKind::DurabilityFinished { status, .. } => {
                groups.push((attempts.clone(), status));
                attempts.clear();
            }
            _ => {}
        }
    }
    groups
}

pub(super) async fn telemetry_durability_test_agent(
    store: Arc<TestCreateStore>,
    telemetry: Arc<TelemetryEmitter>,
    workspace: Arc<WorkspacePolicy>,
) -> TestResult<Agent> {
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 99_010)?;
    Ok(sessions
        .create(
            "create:telemetry-durability",
            durable_artifact_test_builder(workspace, model).telemetry(telemetry),
        )
        .await?)
}

#[tokio::test]
async fn runtime_telemetry_durability_recovers_lost_commit_ack() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let agent =
        telemetry_durability_test_agent(Arc::clone(&store), telemetry, Arc::clone(&workspace))
            .await?;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    store.lose_commit_ack_on(commits_before.saturating_add(1));
    let result = agent.prompt("durability recovery").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    {
        let observations = lock_mutex(&sink.observations);
        let groups = durability_telemetry_groups(&observations);
        assert!(
            groups.iter().any(|(attempts, status)| {
                attempts
                    == &[
                        (
                            DurabilityAttemptKind::InitialCommit,
                            DurabilityAttemptOutcome::Failed,
                        ),
                        (
                            DurabilityAttemptKind::InitialAcknowledgementRecovery,
                            DurabilityAttemptOutcome::RecoveredAcknowledgement,
                        ),
                    ]
                    && *status == TelemetryTerminalStatus::Completed
            }),
            "missing lost-ack recovery durability telemetry group: {groups:?}"
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn runtime_telemetry_durability_fails_when_recovery_misses() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let agent =
        telemetry_durability_test_agent(Arc::clone(&store), telemetry, Arc::clone(&workspace))
            .await?;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    store.lose_commit_ack_on(commits_before.saturating_add(1));
    store.force_recover_commit_not_found();
    if let Ok(turn) = agent.prompt("durability failure").await {
        assert!(
            turn.result().await.is_err(),
            "lost acknowledgement with missing recovery must not settle"
        );
    }
    {
        let observations = lock_mutex(&sink.observations);
        let groups = durability_telemetry_groups(&observations);
        assert!(
            groups.iter().any(|(attempts, status)| {
                attempts
                    == &[
                        (
                            DurabilityAttemptKind::InitialCommit,
                            DurabilityAttemptOutcome::Failed,
                        ),
                        (
                            DurabilityAttemptKind::InitialAcknowledgementRecovery,
                            DurabilityAttemptOutcome::Failed,
                        ),
                    ]
                    && *status == TelemetryTerminalStatus::Failed
            }),
            "missing failed recovery durability telemetry group: {groups:?}"
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn runtime_telemetry_model_first_event_stamps_output_item_not_envelope() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            88_800,
            false,
        )
        .with_ttft_probe(Duration::from_millis(50)),
    );
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:telemetry-ttft",
            durable_artifact_test_builder(Arc::clone(&workspace), model).telemetry(telemetry),
        )
        .await?;
    let result = agent.prompt("measure ttft").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    {
        let observations = lock_mutex(&sink.observations);
        let ttft = observations
            .iter()
            .find_map(|observation| {
                let TelemetryObservationKind::ModelFirstEvent { elapsed } = observation.kind()
                else {
                    return None;
                };
                observation
                    .context()
                    .attempt_id()
                    .is_some()
                    .then_some(elapsed)
            })
            .ok_or("missing ModelFirstEvent with attempt id")?;
        let ttlb = observations
            .iter()
            .find_map(|observation| match observation.kind() {
                TelemetryObservationKind::ModelFinished { elapsed, .. } => Some(elapsed),
                _ => None,
            })
            .ok_or("missing ModelFinished")?;
        assert!(
            ttlb.saturating_sub(ttft) >= Duration::from_millis(45),
            "first output item must stamp before the delayed delta"
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn runtime_telemetry_turn_compaction_finished_carries_turn_context() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            4,
            88_830,
            false,
        )
        .with_tool_call_on(2)
        .with_context_tokens_from_prepare(3, 900_000),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        Arc::clone(&store),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:telemetry-compaction-turn",
            AgentBuilder::new(configured, registry, Arc::clone(&workspace))
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                )
                .with_scheduled_compaction_enabled(true)
                .telemetry(telemetry),
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let turn = agent
        .prompt("use the growing probe at the tool-group fence")
        .await?;
    let _ = turn.result().await?;
    {
        let observations = lock_mutex(&sink.observations);
        assert!(
            observations.iter().any(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::CompactionFinished { .. }
                ) && observation.context().turn_id().is_some()
            }),
            "missing CompactionFinished with turn context"
        );
        let turn_finished = observations.iter().find_map(|observation| {
            let TelemetryObservationKind::TurnFinished {
                compaction_wall,
                compactions,
                ..
            } = observation.kind()
            else {
                return None;
            };
            (compactions >= 1 && compaction_wall > Duration::ZERO).then_some(())
        });
        turn_finished.ok_or("missing TurnFinished compaction compartments")?;
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn runtime_telemetry_local_composition_shutdown_emits_one_child_exit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        authority.clone(),
        0,
        0xc0de_8840,
        false,
    ));
    let composition_descriptor =
        neutral_composition_for(&model, &descriptor, &authority, &registry)?;
    let evidence = Arc::new(FakeCompositionEvidence {
        child_exit_on_integration_shutdown: AtomicBool::new(true),
        ..FakeCompositionEvidence::default()
    });
    let builder = composition_test_builder(
        Arc::clone(&workspace),
        registry,
        &model,
        descriptor,
        authority,
        composition_descriptor,
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::AgentIncarnationPersistent,
        ),
    )?
    .telemetry(telemetry);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        Arc::clone(&store),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:telemetry-child-exit", builder)
        .await?;
    agent.shutdown().await?;
    {
        let observations = lock_mutex(&sink.observations);
        let child_exits = observations
            .iter()
            .filter(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::ChildProcessExited { .. }
                )
            })
            .count();
        assert_eq!(
            child_exits, 1,
            "expected exactly one ChildProcessExited, got {child_exits}"
        );
    }
    Ok(())
}

// --- Tool caller lineage on tool lifecycle observations -----
