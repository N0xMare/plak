#![allow(
    clippy::too_many_lines,
    reason = "Telemetry scenarios compare provider, compaction, retention, and recovered-turn observations against durable authority"
)]

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
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::ToolStarted { .. }
                ) && observation.context().call_id().is_some()
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
                    TelemetryObservationKind::CompactionFinished {
                        measurement,
                        ..
                    } if observation.context().turn_id().is_some()
                        && measurement.context_window_tokens == Some(1_000_000)
                        && measurement.threshold_tokens.is_some()
                        && measurement.occupancy_tokens.is_some()
                        && measurement.idle_gap.is_some()
                )
            }),
            "missing CompactionFinished with window, threshold, occupancy, and the idle gap \
             since the round before its paid call"
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

fn real_measured_registry() -> TestResult<FrozenToolRegistry> {
    use plakous_tools::ToolPack as _;
    let mut registrations = plakous_tool_files::FilesToolPack::default().registrations()?;
    registrations.extend(plakous_tool_search::SearchToolPack.registrations()?);
    registrations.push(plakous_tool_search::SearchToolPack::list_entries_registration()?);
    registrations.extend(Arc::new(ProcessSessionTools::posix_sh()?).registrations()?);
    // The Direct exposure lists tools by name; the configured tools must match.
    registrations.sort_by(|left, right| left.name().cmp(right.name()));
    let mut registry = ToolRegistryBuilder::default();
    for registration in registrations {
        registry.register(registration)?;
    }
    Ok(registry.freeze())
}

fn tool_measurements(observations: &[TelemetryObservation]) -> Vec<crate::ToolMeasurement> {
    observations
        .iter()
        .filter_map(|observation| match observation.kind() {
            TelemetryObservationKind::ToolFinished { measurement, .. } => Some(measurement),
            _ => None,
        })
        .collect()
}

/// The canonical read, search, listing and process tools run for real; every
/// fact comes from their typed values, and no path, query or command text
/// reaches an observation.
#[tokio::test]
async fn runtime_telemetry_measures_the_real_canonical_tools_without_content() -> TestResult {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir(directory.path().join("secret-dir"))?;
    std::fs::write(
        directory.path().join("secret-dir/notes.txt"),
        "one\ntwo\nthree needle-secret\nfour\nfive\n",
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let descriptor = durable_test_descriptor("test.route")?;
    let process_start = json!({
        "command": "PGPASSWORD=hunter2 sleep 3",
        "workdir": ".",
        "login": false,
        "tty": false,
        "keep_stdin_open": false,
        "yield_time_ms": 250,
        "max_output_bytes": 4096
    });
    let process_poll = json!({
        "session_id": 1,
        "yield_time_ms": 0,
        "max_output_bytes": 4096,
        "max_output_records": 16
    });
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            0x6ea5,
            false,
        )
        .with_context_tokens_from_prepare(0, 4_321)
        .with_scripted_tool_calls([
            (
                0,
                "read_file",
                json!({"path": "secret-dir/notes.txt", "offset": 2, "limit": 2}).to_string(),
            ),
            (
                1,
                "read_file",
                json!({"path": "./secret-dir//notes.txt"}).to_string(),
            ),
            (
                2,
                "search_repository",
                json!({"query": "needle-secret", "path": "."}).to_string(),
            ),
            (3, "list_entries", json!({"path": "secret-dir"}).to_string()),
            (4, "process_session_start", process_start.to_string()),
            (5, "process_session_poll", process_poll.to_string()),
        ]),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = real_measured_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )
    .map_err(|error| format!("configure: {error}"))?;
    let agent = sessions
        .create(
            "create:telemetry-measurement",
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
        .await
        .map_err(|error| format!("create: {error}"))?;
    let result = agent
        .prompt("measure the tools")
        .await
        .map_err(|error| format!("prompt: {error}"))?
        .result()
        .await
        .map_err(|error| format!("result: {error}"))?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    agent.shutdown().await?;
    let observations = lock_mutex(&sink.observations).clone();
    let rendered = format!("{observations:?}");
    for secret in ["secret", "hunter2", "PGPASSWORD", "notes", "needle"] {
        assert!(!rendered.contains(secret), "{secret} reached telemetry");
    }
    let tools = tool_measurements(&observations);
    assert_eq!(tools.len(), 6, "{tools:?}");
    let (partial, whole) = (tools[0], tools[1]);
    assert!(partial.access_digest.is_some());
    assert_eq!(partial.access_digest, whole.access_digest, "./a//b is a/b");
    assert_eq!(partial.range_first_line, Some(2));
    assert_eq!(partial.range_lines, Some(2));
    assert_eq!(
        partial.page_partial,
        Some(true),
        "the page stops before EOF"
    );
    assert_eq!(whole.range_first_line, Some(1));
    assert_eq!(whole.page_partial, Some(false));
    let (search, listing) = (tools[2], tools[3]);
    assert!(search.access_digest.is_some() && listing.access_digest.is_some());
    assert_ne!(search.access_digest, partial.access_digest);
    assert_eq!(search.page_partial, Some(false));
    assert_eq!(listing.page_partial, Some(false));
    let start = tools[4];
    assert_eq!(
        start.command_class.map(|class| class.as_str().to_owned()),
        Some("sleep".to_owned())
    );
    assert!(start.wait_kind.is_none() && start.access_digest.is_none());
    let poll = tools[5];
    assert_eq!(poll.wait_count, Some(1));
    assert_eq!(poll.wait_kind, Some(crate::WaitTargetKind::Process));
    assert_eq!(
        poll.wait_outcome,
        Some(crate::WaitOutcome::Timeout),
        "a poll that returns while the process runs ended on its deadline"
    );
    let models: Vec<_> = observations
        .iter()
        .filter_map(|observation| match observation.kind() {
            TelemetryObservationKind::ModelFinished { measurement, .. } => Some(measurement),
            _ => None,
        })
        .collect();
    assert_eq!(models.len(), 7);
    assert!(
        models
            .iter()
            .all(|measurement| measurement.context_window_tokens == Some(1_000_000))
    );
    assert!(models[0].instruction_bytes.is_some() && models[0].tool_schema_bytes.is_some());
    assert!(models[0].tool_schema_bytes > Some(0));
    assert!(models[1..].iter().all(|measurement| {
        measurement.instruction_bytes.is_none() && measurement.tool_schema_bytes.is_none()
    }));
    assert_eq!(models[0].idle_gap, None, "the driver's first request");
    assert!(
        models[1..]
            .iter()
            .all(|measurement| measurement.idle_gap.is_some())
    );
    let prepared: Vec<u64> = models
        .iter()
        .filter_map(|measurement| measurement.prepared_input_tokens)
        .collect();
    let high_water = observations.iter().find_map(|observation| {
        let TelemetryObservationKind::TurnFinished {
            prepared_input_high_water,
            ..
        } = observation.kind()
        else {
            return None;
        };
        Some(prepared_input_high_water)
    });
    assert_eq!(
        high_water.ok_or("missing TurnFinished")?,
        prepared.iter().copied().max()
    );
    Ok(())
}

/// Records whether each call asked its executor for a typed value.
struct WantValueProbe(Arc<Mutex<Vec<(String, bool)>>>);

impl plakous_tools::ToolExecutor for WantValueProbe {
    fn execute(
        self: Arc<Self>,
        invocation: ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        lock_mutex(&self.0).push((
            invocation.identity().name().as_str().to_owned(),
            invocation.want_value(),
        ));
        Box::pin(async { ToolOutcome::success_text("probed") })
    }
}

fn want_value_registry(seen: &Arc<Mutex<Vec<(String, bool)>>>) -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    for (name, field, effect) in [
        ("process_session_start", "command", ToolEffect::Mutating),
        ("read_file", "path", ToolEffect::ReadOnly),
    ] {
        let definition = ToolDefinition::new(
            ToolName::new(name)?,
            "1.0.0",
            "Record the value request.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {field: {"type": "string"}},
                "required": [field],
                "additionalProperties": false
            }))?,
            ToolOutputContract::model_content(128, 4_096)?,
            ToolConcurrency::Serial,
            effect,
        )?;
        registry.register(ToolRegistration::new(
            RegisteredToolId::new(format!("probe.{name}"))?,
            ToolExecutorId::new(format!("probe.{name}.executor"))?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[name.as_bytes()])?,
            definition,
            Arc::new(WantValueProbe(Arc::clone(seen))),
        ))?;
    }
    Ok(registry.freeze())
}

/// Runs one prompt whose rounds make `calls`, reporting to `telemetry`.
async fn scripted_tool_turn(
    create: &str,
    workspace: Arc<WorkspacePolicy>,
    registry: FrozenToolRegistry,
    calls: Vec<(usize, &'static str, String)>,
    telemetry: Arc<TelemetryEmitter>,
    config: AgentConfig,
) -> TestResult<Agent> {
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            0x7e3_0000,
            false,
        )
        .with_scripted_tool_calls(calls),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
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
            create,
            AgentBuilder::new(configured, registry, workspace)
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                )
                .config(config)
                .telemetry(telemetry),
        )
        .await?;
    let result = agent.prompt("call the tools").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    Ok(agent)
}

/// Without a sink no executor is asked for a value, no salt is drawn and no
/// footprint is measured; with one, the measured capabilities ask.
#[tokio::test]
async fn disabled_telemetry_asks_no_executor_for_a_value() -> TestResult {
    for enabled in [false, true] {
        let directory = tempfile::tempdir()?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let telemetry = if enabled {
            Arc::new(TelemetryEmitter::enabled(Arc::new(
                RecordingTelemetrySink {
                    observations: Mutex::new(Vec::new()),
                },
            )))
        } else {
            Arc::new(TelemetryEmitter::disabled())
        };
        let agent = scripted_tool_turn(
            if enabled {
                "create:want-value-enabled"
            } else {
                "create:want-value-disabled"
            },
            Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
            want_value_registry(&seen)?,
            vec![
                (0, "read_file", json!({"path": "a.txt"}).to_string()),
                (
                    1,
                    "process_session_start",
                    json!({"command": "true"}).to_string(),
                ),
            ],
            telemetry,
            AgentConfig::default(),
        )
        .await?;
        assert_eq!(agent.measurement_state_for_test(), (enabled, enabled));
        agent.shutdown().await?;
        assert_eq!(
            *lock_mutex(&seen),
            [
                ("read_file".to_owned(), enabled),
                ("process_session_start".to_owned(), enabled),
            ]
        );
    }
    Ok(())
}

/// A real read over the observation budget is clipped before the model sees
/// it, and that call alone gets one clip observation with its ids.
#[tokio::test]
async fn a_read_over_the_observation_budget_reports_one_clip() -> TestResult {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("large.txt"),
        "0123456789 abcdefghij klmnopqrst uvwxyz\n".repeat(150),
    )?;
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let agent = scripted_tool_turn(
        "create:telemetry-clip",
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        real_measured_registry()?,
        vec![(0, "read_file", json!({"path": "large.txt"}).to_string())],
        Arc::new(TelemetryEmitter::enabled(sink.clone())),
        AgentConfig::default().with_observation_budget_cap_bytes(Some(2_048)),
    )
    .await?;
    agent.shutdown().await?;
    let observations = lock_mutex(&sink.observations).clone();
    let clips: Vec<_> = observations
        .iter()
        .filter_map(|observation| match observation.kind() {
            TelemetryObservationKind::ToolObservationClipped { bytes_dropped, .. } => {
                Some((observation.context(), bytes_dropped))
            }
            _ => None,
        })
        .collect();
    assert_eq!(clips.len(), 1, "{clips:?}");
    let (context, bytes_dropped) = clips[0];
    assert!(bytes_dropped > 0);
    let read = observations
        .iter()
        .find(|observation| {
            matches!(
                observation.kind(),
                TelemetryObservationKind::ToolFinished { .. }
            )
        })
        .ok_or("missing ToolFinished")?
        .context();
    assert!(context.turn_id().is_some() && context.call_id().is_some());
    assert_eq!(
        (context.turn_id(), context.call_id()),
        (read.turn_id(), read.call_id())
    );
    Ok(())
}

/// Content-filter and output-limit retries take the round back; the turn's
/// first attempt still records the request footprint exactly once.
#[tokio::test]
async fn a_retried_first_round_records_its_footprint_once() -> TestResult {
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryEmitter::enabled(sink.clone()));
    let (result, _) = provider_outcome_tests::scripted_turn_with(
        "create:telemetry-retried-footprint",
        provider_outcome_tests::retried_then_answered()?,
        Some(telemetry),
    )
    .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let models: Vec<_> = lock_mutex(&sink.observations)
        .iter()
        .filter_map(|observation| match observation.kind() {
            TelemetryObservationKind::ModelFinished { measurement, .. } => Some(measurement),
            _ => None,
        })
        .collect();
    assert_eq!(models.len(), 3);
    assert_eq!(
        models
            .iter()
            .filter(|measurement| measurement.instruction_bytes.is_some())
            .count(),
        1
    );
    assert!(models[0].instruction_bytes.is_some());
    Ok(())
}

/// A scheduling check that defers stages nothing: a later manual compaction
/// records the route window and no threshold or occupancy.
#[tokio::test]
async fn manual_compaction_after_a_deferred_fence_check_records_no_threshold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let descriptor = durable_test_descriptor("test.route")?;
    // Small measured occupancy: every scheduling check measures and defers.
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            0x7e1_0000,
            false,
        )
        .with_context_tokens_from_prepare(0, 1_000),
    );
    let agent = sessions
        .create(
            "create:telemetry-manual-after-deferred",
            durable_artifact_test_builder(Arc::clone(&workspace), model)
                .with_scheduled_compaction_enabled(true)
                .telemetry(Arc::new(TelemetryEmitter::enabled(sink.clone()))),
        )
        .await?;
    // Every prompt after the first runs the pre-turn scheduling check, which
    // measures the context and defers.
    for prompt in [
        "older replaceable intent",
        "middle intent",
        "latest protected intent",
    ] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    assert!(
        !lock_mutex(&sink.observations)
            .iter()
            .any(|observation| matches!(
                observation.kind(),
                TelemetryObservationKind::CompactionFinished { .. }
            )),
        "the scheduling checks measured and deferred"
    );
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let (builder, _model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:telemetry-manual".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("compact rejected: {error:?}"))?;
    agent.shutdown().await?;
    let manual = lock_mutex(&sink.observations)
        .iter()
        .find_map(|observation| match observation.kind() {
            TelemetryObservationKind::CompactionFinished {
                reason: CompactionEventReason::Manual,
                measurement,
                ..
            } => Some(measurement),
            _ => None,
        })
        .ok_or("missing manual CompactionFinished")?;
    assert_eq!(manual.context_window_tokens, Some(1_000_000));
    assert_eq!(manual.threshold_tokens, None);
    assert_eq!(manual.occupancy_tokens, None);
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
