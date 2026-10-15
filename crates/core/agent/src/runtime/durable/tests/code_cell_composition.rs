use super::*;

#[derive(Default)]
pub(super) struct FakeCompositionEvidence {
    pub(super) counters: CompositionCounters,
    pub(super) session_creates: AtomicUsize,
    pub(super) executes: AtomicUsize,
    pub(super) physical_cell_starts: AtomicUsize,
    pub(super) delay_source: AtomicBool,
    pub(super) source_waiting: Notify,
    pub(super) source_release: Notify,
    pub(super) waits: AtomicUsize,
    pub(super) fail_wait: AtomicBool,
    pub(super) lose_terminate: AtomicBool,
    pub(super) lose_session_shutdown: AtomicBool,
    pub(super) panic_session_shutdown: AtomicBool,
    pub(super) panic_integration_shutdown: AtomicBool,
    pub(super) child_exit_on_integration_shutdown: AtomicBool,
    pub(super) generations: Mutex<Vec<u64>>,
    pub(super) loaded_values: Mutex<Vec<String>>,
    pub(super) order: Mutex<Vec<&'static str>>,
}

pub(super) struct FakeCompositionSession {
    pub(super) evidence: Arc<FakeCompositionEvidence>,
    pub(super) state: Mutex<Option<String>>,
    /// Set by a cell that ends definitely and takes its evaluator with it.
    pub(super) generation_lost: AtomicBool,
}

/// Cell id of the fake cell that yields on every resume and never completes.
pub(super) const FOREVER_YIELD_CELL: &str = "F";

/// Virtual-clock cost the never-finishing cell charges per resume, so the
/// fixture also proves that elapsed time alone never ends the loop.
pub(super) const FOREVER_YIELD_STEP: Duration = Duration::from_secs(240);

/// Round cap the never-finishing cell is run under.
const FOREVER_YIELD_MAX_MODEL_ROUNDS: usize = 8;

/// Scripted resumes offered to the model: far more than the round cap allows.
const FOREVER_YIELD_SCRIPTED_WAITS: usize = 40;

pub(super) struct PendingCompositionEvents;

struct FalseCompositionResponder;

impl crate::local_composition::LocalCompositionNestedResponder for FalseCompositionResponder {
    fn resolve(
        self: Box<Self>,
        _value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async { Ok(crate::local_composition::LocalCompositionNestedDelivery::HostLost) })
    }

    fn reject(
        self: Box<Self>,
        _reason: String,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async { Ok(crate::local_composition::LocalCompositionNestedDelivery::HostLost) })
    }
}

#[tokio::test]
async fn false_nested_result_delivery_requires_started_cell_recovery() {
    let resolved = deliver_local_composition_nested_result(
        Box::new(FalseCompositionResponder),
        Ok(json!({"committed": true})),
    )
    .await
    .expect_err("false resolution delivery must fail the started cell");
    assert_eq!(resolved.kind(), AgentErrorKind::OutcomeUnknown);

    let rejected = deliver_local_composition_nested_result(
        Box::new(FalseCompositionResponder),
        Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "definite nested rejection",
        )),
    )
    .await
    .expect_err("false rejection delivery must fail the started cell");
    assert_eq!(rejected.kind(), AgentErrorKind::OutcomeUnknown);
}

#[test]
fn local_composition_binding_is_dormant_or_exactly_one() {
    validate_local_composition_binding_slot(false)
        .expect("a dormant local-composition slot must remain attachable");
    let duplicate = validate_local_composition_binding_slot(true)
        .expect_err("a second local-composition binding must fail closed");
    assert_eq!(duplicate.kind(), AgentErrorKind::InvalidInput);
    assert_eq!(
        duplicate.message(),
        "an agent may bind only one local composition profile"
    );
}

#[test]
fn persistent_composition_retains_only_a_healthy_idle_owner() {
    use crate::local_composition::LocalCompositionLifecycle;

    assert!(retain_local_composition_at_root_boundary(
        LocalCompositionLifecycle::AgentIncarnationPersistent,
        LocalCompositionRetentionState {
            has_session: true,
            has_events: true,
            has_cell: false,
            possible_start: false,
            has_pending_nested: false,
            has_active_nested: false,
            healthy_idle_observed: true,
        },
    ));
    for unhealthy in [
        (false, true, false, false, false),
        (true, false, false, false, false),
        (true, true, true, false, false),
        (true, true, false, true, false),
        (true, true, false, false, true),
    ] {
        assert!(!retain_local_composition_at_root_boundary(
            LocalCompositionLifecycle::AgentIncarnationPersistent,
            LocalCompositionRetentionState {
                has_session: unhealthy.0,
                has_events: unhealthy.1,
                has_cell: unhealthy.2,
                possible_start: unhealthy.3,
                has_pending_nested: unhealthy.4,
                has_active_nested: false,
                healthy_idle_observed: true,
            },
        ));
    }
    assert!(!retain_local_composition_at_root_boundary(
        LocalCompositionLifecycle::RootTurnJoined,
        LocalCompositionRetentionState {
            has_session: true,
            has_events: true,
            has_cell: false,
            possible_start: false,
            has_pending_nested: false,
            has_active_nested: false,
            healthy_idle_observed: true,
        },
    ));
}

#[test]
fn composition_result_projection_statuses_are_exact_and_distinct() {
    use crate::local_composition::{LocalCompositionResponse, LocalCompositionResultProjection};

    let yielded = LocalCompositionResponse::Yielded {
        cell_id: "cell-7".to_owned(),
        items: Vec::new(),
    };
    let completed = LocalCompositionResponse::Result {
        cell_id: "cell-7".to_owned(),
        items: Vec::new(),
        error: None,
    };
    let failed = LocalCompositionResponse::Result {
        cell_id: "cell-7".to_owned(),
        items: Vec::new(),
        error: Some("bounded".to_owned()),
    };
    let terminated = LocalCompositionResponse::Terminated {
        cell_id: "cell-7".to_owned(),
        items: Vec::new(),
    };
    assert_eq!(
        local_composition_projection_status(
            LocalCompositionResultProjection::TokensScriptV1,
            &yielded,
        ),
        "Script running with cell ID cell-7"
    );
    assert_eq!(
        local_composition_projection_status(
            LocalCompositionResultProjection::BytesPythonPtcV1,
            &yielded,
        ),
        "Python cell running with cell ID cell-7"
    );
    for (response, expected) in [
        (&completed, "Python cell completed"),
        (&failed, "Python cell failed"),
        (&terminated, "Python cell terminated"),
    ] {
        assert_eq!(
            local_composition_projection_status(
                LocalCompositionResultProjection::BytesPythonPtcV1,
                response,
            ),
            expected
        );
    }
}

pub(super) struct FakeCompositionIntegration {
    pub(super) descriptor: LocalCompositionDescriptor,
    pub(super) evidence: Arc<FakeCompositionEvidence>,
    pub(super) lifecycle: LocalCompositionLifecycle,
}

pub(super) fn neutral_composition_test_descriptor() -> TestResult<ResolvedModelDescriptor> {
    let observed_at = Timestamp::new(1, 0)?;
    let base = durable_test_descriptor_at("test.subscription_route", observed_at)?;
    let protocol = ProtocolId::new("test.protocol")?;
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        protocol.clone(),
        RouteId::new("test.subscription_route")?,
        ModelId::new("gpt-5.6-sol")?,
        None,
        Some(ModelFamilyId::new("responses-family")?),
        ModelAdapterProfileId::new("test.compat.2026-07-31.r1")?,
        base.capabilities().clone(),
    )
    .with_context_contract(durable_test_context_contract(protocol, observed_at)?))
}

pub(super) fn neutral_composition_test_registry() -> TestResult<FrozenToolRegistry> {
    let mut builder = ToolRegistryBuilder::default();
    // A fixed plan authority, as a product binds a durable one: the
    // process-local default would make every reopen see a different registry.
    let plan_state = plakous_tool_plan::InMemoryPlanState::with_identity(
        plakous_tools::PlanStateIdentity::new("test.neutral-composition.plan", 1)?,
    );
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs(
            Arc::new(plan_state),
            Arc::new(ProcessSessionTools::posix_sh()?),
        ),
        &mut builder,
    )?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    Ok(builder.freeze())
}

pub(super) fn neutral_composition_descriptor(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
) -> TestResult<LocalCompositionDescriptor> {
    let exact =
        CodexNativeLocalCodeModeV1::compile(plakous_tools::JobConfig::default(), registry, target)?;
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.exposed_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: "exec".into(),
        wait_name: "wait".into(),
        marker_executor: ToolExecutorId::new("plakous.codex-code-mode.driver-marker.v1")?,
    })
}

pub(super) fn composition_test_builder(
    workspace: Arc<WorkspacePolicy>,
    registry: FrozenToolRegistry,
    model: &Arc<ArtifactModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    composition_descriptor: LocalCompositionDescriptor,
    integration_profile: (Arc<FakeCompositionEvidence>, LocalCompositionLifecycle),
) -> TestResult<AgentBuilder> {
    let (evidence, lifecycle) = integration_profile;
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&["exec", "wait"]),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(FakeComposition::Neutral(FakeCompositionIntegration {
            descriptor: composition_descriptor.clone(),
            evidence,
            lifecycle,
        }));
    let profile = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("test.persistent-local-composition")?)
        .semantic_version(ProfileSemanticVersion::new(1, 0, 0))
        .provenance(HarnessProfileProvenance::Synthetic)
        .base_instructions("")
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .reasoning_strategy(ReasoningPreferenceStrategy::Disabled)
        .continuation_strategy(ContinuationPreferenceStrategy::CanonicalReplayOnly)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()?;
    let plan = compile_explicit_runtime_profile(
        &profile,
        model.as_ref(),
        configured.configuration(),
        &registry,
        workspace.as_ref(),
        Vec::new(),
    )?;
    let attachment_identity = composition_descriptor.identity;
    let mut attachment = LocalCompositionAttachmentIdentity {
        profile: attachment_identity,
        artifact: attachment_identity,
        binding: attachment_identity,
        engine_contract: attachment_identity,
        evidence_receipt: None,
    };
    attachment.binding = local_composition_binding_identity(&attachment)?;
    Ok(AgentBuilder::new(configured, registry, workspace)
        .local_programmatic_composition(LocalCompositionConstruction {
            descriptor: composition_descriptor,
            integration,
            attachment,
        })?
        .harness_plan(plan)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        ))
}

#[tokio::test]
async fn local_composition_invalid_wait_then_valid_wait_completes_without_replay() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5000, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("yield once")?),
                (
                    1,
                    "wait",
                    serde_json::to_string(&json!({
                        "cell_id": "C",
                        "yield_time_ms": 0.5
                    }))?,
                ),
                (
                    2,
                    "wait",
                    serde_json::to_string(&json!({
                        "cell_id": "C",
                        "yield_time_ms": 10,
                        "max_tokens": 128,
                        "terminate": false
                    }))?,
                ),
                (
                    3,
                    "wait",
                    serde_json::to_string(&json!({
                        "cell_id": "C",
                        "yield_time_ms": 10,
                        "max_tokens": 128,
                        "terminate": false
                    }))?,
                ),
            ]),
    );
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&["exec", "wait"]),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let effective = configured.configuration().effective();
    let target = ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        descriptor
            .model_family()
            .ok_or("neutral composition descriptor omitted its family")?
            .as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let composition_descriptor = neutral_composition_descriptor(registry.clone(), target)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(FakeComposition::Neutral(FakeCompositionIntegration {
            descriptor: composition_descriptor.clone(),
            evidence: Arc::clone(&evidence),
            lifecycle: LocalCompositionLifecycle::RootTurnJoined,
        }));
    let profile = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("test.neutral-local-composition")?)
        .semantic_version(ProfileSemanticVersion::new(1, 0, 0))
        .provenance(HarnessProfileProvenance::Synthetic)
        .base_instructions("")
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .reasoning_strategy(ReasoningPreferenceStrategy::Disabled)
        .continuation_strategy(ContinuationPreferenceStrategy::CanonicalReplayOnly)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()?;
    let plan = compile_explicit_runtime_profile(
        &profile,
        model.as_ref(),
        configured.configuration(),
        &registry,
        workspace.as_ref(),
        Vec::new(),
    )?;
    let attachment_identity = composition_descriptor.identity;
    let mut attachment = LocalCompositionAttachmentIdentity {
        profile: attachment_identity,
        artifact: attachment_identity,
        binding: attachment_identity,
        engine_contract: attachment_identity,
        evidence_receipt: None,
    };
    attachment.binding = local_composition_binding_identity(&attachment)?;
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let builder = AgentBuilder::new(configured, registry, workspace)
        .local_programmatic_composition(LocalCompositionConstruction {
            descriptor: composition_descriptor,
            integration,
            attachment,
        })?
        .harness_plan(plan)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .telemetry(Arc::new(TelemetryEmitter::enabled(sink.clone())));
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store;
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:neutral-composition-yield-wait", builder)
        .await?;

    let mut turn = agent.prompt("exercise neutral composition").await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    let mut started_names = Vec::new();
    let mut completed_names = Vec::new();
    while let Ok(event) = events.recv().await {
        match event.kind() {
            AgentEventKind::ToolStarted { identity, .. } => {
                started_names.push(identity.canonical_name().as_str().to_owned());
            }
            AgentEventKind::ToolCompleted { identity, .. } => {
                completed_names.push(identity.canonical_name().as_str().to_owned());
            }
            _ => {}
        }
    }
    assert!(
        started_names.iter().any(|name| name == "exec"),
        "outer composition exec must publish ToolStarted: {started_names:?}"
    );
    assert!(
        started_names.iter().any(|name| name == "wait"),
        "outer composition wait must publish ToolStarted: {started_names:?}"
    );
    assert!(
        completed_names.iter().any(|name| name == "exec"),
        "outer composition exec must publish ToolCompleted: {completed_names:?}"
    );
    assert!(
        completed_names.iter().any(|name| name == "wait"),
        "outer composition wait must publish ToolCompleted: {completed_names:?}"
    );
    // Outer exec/wait skip ToolRuntime, so the telemetry pair must come
    // from the composition path itself: one Started and one Finished per
    // model-visible call (exec, the rejected wait, the valid wait), each
    // with its own call id and its real terminal status.
    let (mut tool_started, mut tool_finished, mut statuses) = (Vec::new(), Vec::new(), Vec::new());
    let mut turn_tool_calls = None;
    for observation in lock_mutex(&sink.observations).iter() {
        match observation.kind() {
            TelemetryObservationKind::ToolStarted => {
                tool_started.push(observation.context().call_id());
            }
            TelemetryObservationKind::ToolFinished { status, .. } => {
                statuses.push(status);
                tool_finished.push(observation.context().call_id());
            }
            TelemetryObservationKind::TurnFinished { tool_calls, .. } => {
                turn_tool_calls = Some(tool_calls);
            }
            _ => {}
        }
    }
    assert_eq!(
        tool_started.len(),
        3,
        "exec + rejected wait + wait: {tool_started:?}"
    );
    assert_eq!(tool_started, tool_finished);
    assert!(tool_started.iter().all(Option::is_some));
    let distinct: BTreeSet<_> = tool_started.iter().collect();
    assert_eq!(distinct.len(), 3);
    assert_eq!(
        statuses,
        [
            TelemetryTerminalStatus::Completed,
            TelemetryTerminalStatus::Failed,
            TelemetryTerminalStatus::Completed,
        ]
    );
    assert_eq!(turn_tool_calls, Some(3));
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected neutral composition outcome: {:?}; evidence order: {:?}; model calls {}, execs {}, waits {}, terminates {}, shutdowns {}",
        result.outcome(),
        lock_mutex(&evidence.order),
        model.calls.load(Ordering::Acquire),
        evidence.executes.load(Ordering::Acquire),
        evidence.waits.load(Ordering::Acquire),
        evidence.counters.terminates.load(Ordering::Acquire),
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 5);
    assert_eq!(evidence.executes.load(Ordering::Acquire), 1);
    assert_eq!(evidence.waits.load(Ordering::Acquire), 1);
    assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        1
    );
    assert_eq!(
        lock_mutex(&evidence.order).as_slice(),
        ["create-session", "execute", "wait", "session-shutdown"]
    );
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert!(result.recovery_operation().is_none());

    let evaluations = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(evaluations.len(), 1);
    let evaluation_operation_id = evaluations[0].operation_id();
    let mut exec_evaluations = 0;
    let mut invalid_waits = 0;
    let mut valid_waits = 0;
    for evaluation in evaluations {
        let request = decode_local_composition_evaluation_request(evaluation.request())
            .ok_or("composition evaluation request did not decode")?;
        let transitions = evaluation.transitions();
        assert!(matches!(
            evaluation.state(),
            DurableOperationState::Finalized(_)
        ));
        match request.call.canonical_name().as_str() {
            "exec" => {
                exec_evaluations += 1;
                assert_eq!(
                    transitions
                        .iter()
                        .filter(|transition| {
                            matches!(transition.state(), DurableOperationState::Dispatched)
                        })
                        .count(),
                    1
                );
                assert_eq!(
                    transitions
                        .iter()
                        .filter(|transition| {
                            matches!(transition.state(), DurableOperationState::LocatorBound(_))
                        })
                        .count(),
                    1
                );
            }
            other => return Err(format!("unexpected composition marker {other}").into()),
        }
    }
    let continuations = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionContinuation { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(continuations.len(), 2);
    for continuation in continuations {
        let DurableOperationKind::CompositionContinuation {
            evaluation_operation_id: parent,
            ..
        } = continuation.kind()
        else {
            unreachable!()
        };
        assert_eq!(*parent, evaluation_operation_id);
        let request = decode_local_composition_continuation_request(continuation.request())
            .ok_or("composition continuation request did not decode")?;
        assert_eq!(request.evaluation_operation_id, evaluation_operation_id);
        let transitions = continuation.transitions();
        if transitions.iter().any(|transition| {
            matches!(
                transition.state(),
                DurableOperationState::ProvenNotDispatched(_)
            )
        }) {
            invalid_waits += 1;
            assert!(!transitions.iter().any(|transition| {
                matches!(transition.state(), DurableOperationState::Dispatched)
            }));
        } else {
            valid_waits += 1;
            assert_eq!(
                transitions
                    .iter()
                    .filter(|transition| {
                        matches!(transition.state(), DurableOperationState::Dispatched)
                    })
                    .count(),
                1
            );
        }
    }
    assert_eq!((exec_evaluations, invalid_waits, valid_waits), (1, 1, 1));

    agent.shutdown().await?;
    assert_eq!(
        evidence
            .counters
            .integration_shutdowns
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(
        lock_mutex(&evidence.order).as_slice(),
        [
            "create-session",
            "execute",
            "wait",
            "session-shutdown",
            "integration-shutdown"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn yielded_wait_failure_joins_once_and_preserves_cleanup_pair_failures() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5050, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("yield once")?),
                (
                    1,
                    "wait",
                    serde_json::to_string(&json!({
                        "cell_id": "C",
                        "yield_time_ms": 10,
                        "max_tokens": 128,
                        "terminate": false
                    }))?,
                ),
            ]),
    );
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&["exec", "wait"]),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let effective = configured.configuration().effective();
    let target = ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        descriptor
            .model_family()
            .ok_or("neutral composition descriptor omitted its family")?
            .as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let composition_descriptor = neutral_composition_descriptor(registry.clone(), target)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    evidence.fail_wait.store(true, Ordering::Release);
    evidence.lose_terminate.store(true, Ordering::Release);
    evidence
        .lose_session_shutdown
        .store(true, Ordering::Release);
    let builder = composition_test_builder(
        workspace,
        registry,
        &model,
        descriptor,
        authority,
        composition_descriptor,
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::RootTurnJoined,
        ),
    )?;
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:composition-wait-failure", builder)
        .await?;

    let result = agent.prompt("yield then lose wait").await?.result().await?;
    let TurnResultOutcome::RecoveryRequired(error) = result.outcome() else {
        return Err(format!("unexpected wait failure outcome: {:?}", result.outcome()).into());
    };
    assert!(
        error.message().contains("injected wait transport failure"),
        "unexpected recovery diagnostic: {}",
        error.message()
    );
    assert!(error.message().contains("injected terminate loss"));
    assert!(error.message().contains("injected session shutdown loss"));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(evidence.executes.load(Ordering::Acquire), 1);
    assert_eq!(evidence.physical_cell_starts.load(Ordering::Acquire), 1);
    assert_eq!(evidence.waits.load(Ordering::Acquire), 1);
    assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        1
    );
    assert_eq!(
        lock_mutex(&evidence.order).as_slice(),
        [
            "create-session",
            "execute",
            "wait",
            "terminate",
            "session-shutdown",
            "integration-shutdown"
        ]
    );
    let operations = result.checkpoint().manifest().ledger().operations();
    assert_eq!(
        operations
            .iter()
            .filter(|record| matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
            ))
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|record| matches!(
                record.kind(),
                DurableOperationKind::CompositionContinuation { .. }
            ))
            .count(),
        1
    );

    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cleanup_panics_still_attempt_integration_release_and_flush() -> TestResult {
    for session_panic in [true, false] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let registry = neutral_composition_test_registry()?;
        let descriptor = neutral_composition_test_descriptor()?;
        let authority = artifact_authority(&descriptor, 1)?;
        let model = Arc::new(
            ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5100, false)
                .with_scripted_tool_calls([
                    (0, "exec", serde_json::to_string("store bounded state")?),
                    (2, "exec", serde_json::to_string("load bounded state")?),
                ]),
        );
        let configured = configured_test_model_for_operation_with_tools(
            model.clone(),
            descriptor.clone(),
            authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
            1,
            ReasoningEffortDisposition::Disabled,
            Some(&["exec", "wait"]),
            ToolPresentationMode::LocalCompositionOnly,
            false,
        )?;
        let effective = configured.configuration().effective();
        let target = ProviderModelProjectionIdentityV2::new(
            descriptor.integration_class(),
            descriptor.protocol().as_str(),
            descriptor.route().as_str(),
            descriptor.profile().as_str(),
            descriptor
                .model_family()
                .ok_or("neutral composition descriptor omitted its family")?
                .as_str(),
            descriptor.requested_model().as_str(),
            effective.effective_model().as_str(),
            configured.configuration().effective_identity(),
            plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        let composition_descriptor = neutral_composition_descriptor(registry.clone(), target)?;
        let evidence = Arc::new(FakeCompositionEvidence::default());
        let builder = composition_test_builder(
            Arc::clone(&workspace),
            registry.clone(),
            &model,
            descriptor.clone(),
            authority.clone(),
            composition_descriptor.clone(),
            (
                Arc::clone(&evidence),
                LocalCompositionLifecycle::AgentIncarnationPersistent,
            ),
        )?;
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let agent = sessions
            .create("create:persistent-local-composition", builder)
            .await?;

        let first = agent
            .prompt("store evaluator state")
            .await?
            .result()
            .await?;
        assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
        let first_observation = first
            .checkpoint()
            .manifest()
            .interpreter_observation()
            .ok_or("first root omitted interpreter observation")?;
        assert_eq!(first_observation.generation(), 1);
        assert_eq!(
            first_observation.availability(),
            InterpreterAvailability::Available
        );
        assert_eq!(
            first_observation.last_acknowledged(),
            Some(InterpreterAcknowledgedBoundary::Terminal)
        );
        assert_eq!(evidence.session_creates.load(Ordering::Acquire), 1);
        assert_eq!(
            evidence.counters.session_shutdowns.load(Ordering::Acquire),
            0
        );

        evidence
            .panic_session_shutdown
            .store(session_panic, Ordering::Release);
        evidence
            .panic_integration_shutdown
            .store(!session_panic, Ordering::Release);
        let releases = store.releases.load(Ordering::Acquire);
        let flushes = store.flushes.load(Ordering::Acquire);
        assert!(agent.shutdown().await.is_err());
        assert_eq!(
            evidence
                .counters
                .integration_shutdowns
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(store.releases.load(Ordering::Acquire), releases + 1);
        assert_eq!(store.flushes.load(Ordering::Acquire), flushes + 1);
        assert!(lock_mutex(&store.state).active_lease.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn a_definite_cell_outcome_that_loses_its_generation_still_completes_the_turn() -> TestResult
{
    // A cell can end definitely and still cost the interpreter its evaluator.
    // The known outcome is delivered and the turn completes; the generation is
    // reset under a typed reason, and the next cell runs on a fresh evaluator
    // rather than re-evaluating anything.
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5400, false)
            .with_scripted_tool_calls([
                (
                    0,
                    "exec",
                    serde_json::to_string("raise and lose the generation")?,
                ),
                (2, "exec", serde_json::to_string("load bounded state")?),
            ]),
    );
    let composition_descriptor = {
        let configured = configured_test_model_for_operation_with_tools(
            model.clone(),
            descriptor.clone(),
            authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
            1,
            ReasoningEffortDisposition::Disabled,
            Some(&["exec", "wait"]),
            ToolPresentationMode::LocalCompositionOnly,
            false,
        )?;
        let effective = configured.configuration().effective();
        let target = ProviderModelProjectionIdentityV2::new(
            descriptor.integration_class(),
            descriptor.protocol().as_str(),
            descriptor.route().as_str(),
            descriptor.profile().as_str(),
            descriptor
                .model_family()
                .ok_or("neutral composition descriptor omitted its family")?
                .as_str(),
            descriptor.requested_model().as_str(),
            effective.effective_model().as_str(),
            configured.configuration().effective_identity(),
            plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        neutral_composition_descriptor(registry.clone(), target)?
    };
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = composition_test_builder(
        Arc::clone(&workspace),
        registry.clone(),
        &model,
        descriptor.clone(),
        authority.clone(),
        composition_descriptor,
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::AgentIncarnationPersistent,
        ),
    )?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:lost-composition-generation", builder)
        .await?;

    let first = agent.prompt("raise in a cell").await?.result().await?;
    assert!(
        matches!(first.outcome(), TurnResultOutcome::Completed(_)),
        "a definite cell failure was turned into a lost turn: {:?}",
        first.outcome()
    );
    let observation = first
        .checkpoint()
        .manifest()
        .interpreter_observation()
        .ok_or("the first root omitted its interpreter observation")?;
    assert_eq!(observation.generation(), 1);
    assert_eq!(observation.availability(), InterpreterAvailability::Reset);
    assert_eq!(
        observation.reset_reason(),
        Some(InterpreterResetReason::ProtocolViolation)
    );
    assert_eq!(observation.affected_cell(), None);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        1
    );
    let delivered = first
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(ToolResultBlock::parts)
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => Some(text.text().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        delivered.iter().any(|text| text.contains("cell raised")),
        "the cell's own failure was not reported to the model: {delivered:?}"
    );
    assert!(
        delivered.iter().any(|text| text == "printed"),
        "output printed before the failure was lost: {delivered:?}"
    );

    let second = agent.prompt("run another cell").await?.result().await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "the next cell could not run after the reset: {:?}",
        second.outcome()
    );
    assert_eq!(evidence.session_creates.load(Ordering::Acquire), 2);
    assert_eq!(evidence.executes.load(Ordering::Acquire), 2);
    assert_eq!(
        *lock_mutex(&evidence.loaded_values),
        vec!["missing".to_owned()],
        "the reset generation carried state, or source was replayed"
    );
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn empty_text_item_is_dropped_and_never_fails_the_cell() -> TestResult {
    // User code may emit an empty text item (`text("")` in a JS cell, or a
    // Python cell valued at the empty string). It carries no model content and
    // no canonical text block can hold it, so the projection drops it and the
    // turn completes with the surrounding output delivered.
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5300, false)
            .with_scripted_tool_calls([(0, "exec", serde_json::to_string("emit empty text")?)]),
    );
    let composition_descriptor = {
        let configured = configured_test_model_for_operation_with_tools(
            model.clone(),
            descriptor.clone(),
            authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
            1,
            ReasoningEffortDisposition::Disabled,
            Some(&["exec", "wait"]),
            ToolPresentationMode::LocalCompositionOnly,
            false,
        )?;
        let effective = configured.configuration().effective();
        let target = ProviderModelProjectionIdentityV2::new(
            descriptor.integration_class(),
            descriptor.protocol().as_str(),
            descriptor.route().as_str(),
            descriptor.profile().as_str(),
            descriptor
                .model_family()
                .ok_or("neutral composition descriptor omitted its family")?
                .as_str(),
            descriptor.requested_model().as_str(),
            effective.effective_model().as_str(),
            configured.configuration().effective_identity(),
            plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        neutral_composition_descriptor(registry.clone(), target)?
    };
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = composition_test_builder(
        Arc::clone(&workspace),
        registry.clone(),
        &model,
        descriptor.clone(),
        authority.clone(),
        composition_descriptor,
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::RootTurnJoined,
        ),
    )?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:empty-composition-text", builder)
        .await?;
    let result = agent
        .prompt("emit an empty text item")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "empty text output failed the turn: {:?}",
        result.outcome()
    );
    let delivered = result
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(ToolResultBlock::parts)
        .map(|part| match part {
            ToolResultPart::Text(text) => text.text().to_owned(),
            other => panic!("unexpected non-text composition part: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        delivered.iter().any(|text| text == "kept output"),
        "the ordinary text item was not delivered: {delivered:?}"
    );
    assert!(
        delivered.iter().all(|text| !text.is_empty()),
        "an empty canonical text part reached the conversation: {delivered:?}"
    );
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn successive_composition_resumes_are_bounded_by_the_round_cap() -> TestResult {
    // A composition cell that answers every resume successfully has no budget
    // of its own and no turn wall clock bounds it: each resume is a model
    // round, so the round cap ends the loop however long the resumes take.
    // Every resume charges four virtual minutes, so the turn runs past the
    // retired 20-minute deadline before the cap stops it.
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let mut scripted = vec![(0, "exec", serde_json::to_string("yield forever")?)];
    for round in 1..=FOREVER_YIELD_SCRIPTED_WAITS {
        scripted.push((
            round,
            "wait",
            serde_json::to_string(&json!({
                "cell_id": FOREVER_YIELD_CELL,
                "yield_time_ms": FOREVER_YIELD_STEP.as_millis(),
                "max_tokens": 128,
                "terminate": false
            }))?,
        ));
    }
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5200, false)
            .with_scripted_tool_calls(scripted),
    );
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&["exec", "wait"]),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let effective = configured.configuration().effective();
    let target = ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        descriptor
            .model_family()
            .ok_or("neutral composition descriptor omitted its family")?
            .as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let composition_descriptor = neutral_composition_descriptor(registry.clone(), target)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = composition_test_builder(
        Arc::clone(&workspace),
        registry,
        &model,
        descriptor,
        authority,
        composition_descriptor,
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::RootTurnJoined,
        ),
    )?
    .config(AgentConfig::default().with_max_model_rounds(FOREVER_YIELD_MAX_MODEL_ROUNDS));
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        Arc::clone(&store),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:composition-root-deadline", builder)
        .await?;

    let started = tokio::time::Instant::now();
    let turn = agent.prompt("yield forever").await?.result().await?;
    let TurnResultOutcome::Failed(error) = turn.outcome() else {
        return Err(format!("expected the round limit, got {:?}", turn.outcome()).into());
    };
    assert_eq!(error.kind(), AgentErrorKind::RoundLimit, "{error:?}");
    // Round 1 is `exec`; rounds 2..=cap are resumes; the wrap-up round's
    // scripted `wait` is refused rather than dispatched.
    assert_eq!(
        evidence.waits.load(Ordering::Acquire),
        FOREVER_YIELD_MAX_MODEL_ROUNDS - 1,
        "the round cap, not the scripted resume count, must end the loop"
    );
    assert!(started.elapsed() > Duration::from_secs(1_200));
    let _shutdown = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn persistent_local_composition_state_survives_compaction_between_cells() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5100, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("store bounded state")?),
                (3, "exec", serde_json::to_string("load bounded state")?),
            ]),
    );
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&["exec", "wait"]),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let effective = configured.configuration().effective();
    let target = ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        descriptor
            .model_family()
            .ok_or("neutral composition descriptor omitted its family")?
            .as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let composition_descriptor = neutral_composition_descriptor(registry.clone(), target)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = composition_test_builder(
        Arc::clone(&workspace),
        registry.clone(),
        &model,
        descriptor.clone(),
        authority.clone(),
        composition_descriptor.clone(),
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::AgentIncarnationPersistent,
        ),
    )?;
    let reopen_builder = composition_test_builder(
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
    )?;
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        Arc::clone(&store),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:persistent-local-composition", builder)
        .await?;

    let first = agent
        .prompt("store evaluator state")
        .await?
        .result()
        .await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    let first_observation = first
        .checkpoint()
        .manifest()
        .interpreter_observation()
        .ok_or("first root omitted interpreter observation")?;
    assert_eq!(first_observation.generation(), 1);
    assert_eq!(
        first_observation.availability(),
        InterpreterAvailability::Available
    );
    assert_eq!(
        first_observation.last_acknowledged(),
        Some(InterpreterAcknowledgedBoundary::Terminal)
    );
    assert_eq!(evidence.session_creates.load(Ordering::Acquire), 1);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        0
    );

    // Compaction protects the latest conversation range. Supply a separate
    // ordinary root so the first cell's completed range is eligible.
    let protected_tail = agent
        .prompt("retain the latest conversation range")
        .await?
        .result()
        .await?;
    assert!(matches!(
        protected_tail.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert_eq!(evidence.executes.load(Ordering::Acquire), 1);

    let compaction_model = RecoveryCompactionModel::new(
        model.descriptor.clone(),
        model.authority.clone(),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let compaction_configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        compaction_model.clone(),
        first
            .checkpoint()
            .manifest()
            .provider_configuration()
            .clone(),
        model.authority.clone(),
    )?;
    let compaction_builder = AgentBuilder::new(
        compaction_configured,
        neutral_composition_test_registry()?,
        Arc::clone(&workspace),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
        ReasoningPolicy::default(),
    ))
    .identity_sources(
        Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
        Arc::new(SystemIdGenerator),
    );
    let compacted = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:persistent-composition".to_owned(),
            agent.session_id(),
            agent.branch_id(),
            protected_tail.checkpoint_id(),
            AgentCompactionReason::Manual,
            compaction_builder,
        ))
        .await
        .map_err(|error| format!("persistent compaction rejected: {error:?}"))?;
    assert!(
        matches!(compacted, AgentCompactSessionOutcome::Compacted { .. }),
        "{compacted:?}"
    );
    assert_eq!(compaction_model.starts.load(Ordering::Acquire), 1);
    assert_eq!(evidence.session_creates.load(Ordering::Acquire), 1);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        0
    );
    assert_eq!(
        agent.checkpoint().manifest().interpreter_observation(),
        Some(first_observation)
    );

    let second = agent.prompt("load evaluator state").await?.result().await?;
    assert!(matches!(second.outcome(), TurnResultOutcome::Completed(_)));
    let second_observation = second
        .checkpoint()
        .manifest()
        .interpreter_observation()
        .ok_or("second root omitted interpreter observation")?;
    assert_eq!(second_observation.generation(), 1);
    assert_eq!(
        second_observation.availability(),
        InterpreterAvailability::Available
    );
    assert_eq!(
        second_observation.last_acknowledged(),
        Some(InterpreterAcknowledgedBoundary::Terminal)
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 5);
    assert_eq!(evidence.executes.load(Ordering::Acquire), 2);
    assert_eq!(evidence.physical_cell_starts.load(Ordering::Acquire), 2);
    assert_eq!(evidence.session_creates.load(Ordering::Acquire), 1);
    assert_eq!(lock_mutex(&evidence.generations).as_slice(), [1]);
    assert_eq!(lock_mutex(&evidence.loaded_values).as_slice(), ["survived"]);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        0
    );

    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        second.checkpoint_id(),
    );

    agent.shutdown().await?;
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        1
    );
    assert_eq!(
        evidence
            .counters
            .integration_shutdowns
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(
        lock_mutex(&evidence.order).as_slice(),
        [
            "create-session",
            "execute",
            "execute",
            "session-shutdown",
            "integration-shutdown"
        ]
    );

    let opened = sessions
        .open("open:persistent-local-composition", address, reopen_builder)
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("quiescent persistent composition unexpectedly required recovery".into());
    };
    let reopened = opened
        .prompt("observe process reopen without evaluating source")
        .await?
        .result()
        .await?;
    assert!(matches!(
        reopened.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let reset = reopened
        .checkpoint()
        .manifest()
        .interpreter_observation()
        .ok_or("reopened root omitted interpreter reset")?;
    assert_eq!(reset.generation(), 2);
    assert_eq!(reset.availability(), InterpreterAvailability::Reset);
    assert_eq!(
        reset.reset_reason(),
        Some(InterpreterResetReason::ProcessReopen)
    );
    assert_eq!(reset.affected_cell(), None);
    assert_eq!(reset.last_acknowledged(), None);
    assert_eq!(evidence.executes.load(Ordering::Acquire), 2);
    assert_eq!(evidence.physical_cell_starts.load(Ordering::Acquire), 2);
    assert_eq!(evidence.session_creates.load(Ordering::Acquire), 1);
    assert_eq!(lock_mutex(&evidence.generations).as_slice(), [1]);
    opened.shutdown().await?;
    Ok(())
}
