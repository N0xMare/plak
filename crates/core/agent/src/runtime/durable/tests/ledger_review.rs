use super::*;

struct SessionPairs {
    calls: AtomicUsize,
    tools: [Arc<SessionUpdateTool>; 2],
}

impl plakous_tools::ToolExecutor for SessionPairs {
    fn execute(
        self: Arc<Self>,
        invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        let pair = self.calls.fetch_add(1, Ordering::AcqRel) / 2;
        plakous_tools::ToolExecutor::execute(self.tools[pair].clone(), invocation, context)
    }
}

fn probe_registration(
    executor: Arc<dyn plakous_tools::ToolExecutor>,
    concurrency: ToolConcurrency,
    effect: ToolEffect,
) -> TestResult<ToolRegistration> {
    Ok(ToolRegistration::new(
        RegisteredToolId::new("review.probe")?,
        ToolExecutorId::new("review.probe")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"review.probe"])?,
        ToolDefinition::new(
            ToolName::new("probe")?,
            "1.0.0",
            "Exercise nested ledger boundaries.",
            ToolInputContract::structured(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object","properties":{},"required":[],"additionalProperties":false
            }))?,
            ToolOutputContract::model_content(128, 1_048_576)?,
            concurrency,
            effect,
        )?,
        executor,
    )
    .nested_eligible())
}

pub(super) fn review_composition(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    direct_probe: bool,
) -> TestResult<LocalCompositionDescriptor> {
    let exact = plakous_compose::CodexMinimalLocalCodeModeV1::compile_with_extensions(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
        &[ToolName::new("probe")?],
    )?;
    let mut entries = exact.outer_projection().entries().to_vec();
    if direct_probe {
        entries.push(
            exact
                .nested_projection()
                .by_exposed(&ToolName::new("probe")?)
                .ok_or("probe projection")?
                .clone(),
        );
    }
    Ok(LocalCompositionDescriptor {
        outer_projection: plakous_tools::internal::projection_map(registry, target, entries)?,
        nested_projection: exact.nested_projection().clone(),
        nested_tools: exact
            .nested_tools()
            .iter()
            .map(|tool| LocalCompositionToolDefinition {
                name: tool.exposed_name().as_str().to_owned(),
                description: tool.description().to_owned(),
                input_schema: tool.composition_input_schema_value(),
            })
            .collect::<Vec<_>>()
            .into(),
        identity: exact.identity(),
        execute_name: "exec".into(),
        wait_name: "wait".into(),
        marker_executor: ToolExecutorId::new("plakous.codex-code-mode.driver-marker.v1")?,
    })
}

async fn reserved_agent(
    directory: &std::path::Path,
) -> TestResult<(Agent, Arc<FakeCompositionEvidence>)> {
    let registry = neutral_composition_registry_with_tools(
        false,
        vec![probe_registration(
            Arc::new(plakous_test_support::ScriptedTool::new(
                (0..6).map(|_| ToolOutcome::success_text("ok")),
            )),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?],
    )?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let mut calls = vec![("probe".to_owned(), "{}".to_owned()); 6];
    calls.push(("exec".to_owned(), serde_json::to_string("seed job")?));
    let model = ledger_retirement::batch_model_for_calls(&descriptor, &authority, [calls])?;
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        None,
        ToolPresentationMode::Direct,
        false,
    )?;
    let target = composition_projection_target(&configured, &descriptor)?;
    let composition = review_composition(registry.clone(), target, true)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let integration = Arc::new(FakeComposition::Neutral(FakeCompositionIntegration {
        descriptor: composition.clone(),
        evidence: evidence.clone(),
        lifecycle: LocalCompositionLifecycle::RootTurnJoined,
    }));
    let builder = code_cell_agent_builder(
        model.as_ref(),
        configured,
        registry,
        Arc::new(WorkspacePolicy::host(directory, ".")?),
        "test.reserved-top-level",
        composition,
        integration,
    )?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    Ok((
        sessions
            .create("create:reserved-top-level", builder)
            .await?,
        evidence,
    ))
}

#[tokio::test]
async fn nested_headroom_charges_the_queued_top_level_readonly_calls() -> TestResult {
    if std::env::var_os("PLAK_TEST_LEDGER_RESERVED_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::nested_headroom_charges_the_queued_top_level_readonly_calls"
        )
        .strip_prefix("plakous_agent::")
        .ok_or("test name")?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", name, "--nocapture"])
            .env("PLAK_TEST_LEDGER_RESERVED_CHILD", "1")
            .env("PLAK_TEST_LEDGER_HEADROOM_LIMIT", "36")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("sample"), "read only")?;
    let (agent, evidence) = reserved_agent(directory.path())
        .await
        .map_err(|error| format!("reserved fixture: {error}"))?;
    let turn = agent.prompt("queue readers before a mutating cell").await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while evidence.physical_cell_starts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let before = agent.checkpoint();
    assert!(
        before.manifest().ledger().operation_count() + 4 < 36,
        "unreserved nested work alone must fit"
    );
    let deliveries = Arc::new(CancelCellEvidence::default());
    nested(
        &evidence,
        &deliveries,
        "refused",
        "exec_command",
        json!({"cmd":"printf must-not-run"}),
    )?;
    delivered(&deliveries, 1).await?;
    let values = lock_mutex(&deliveries.deliveries).clone();
    assert!(
        !values[0].1,
        "count={}, deliveries={values:?}",
        before.manifest().ledger().operation_count()
    );
    assert!(
        values[0]
            .2
            .contains(crate::runtime::operation_record::NESTED_HEADROOM_REFUSAL)
    );
    assert_eq!(
        agent.checkpoint().manifest().ledger().operation_count(),
        before.manifest().ledger().operation_count()
    );
    evidence.source_release.notify_one();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    agent.shutdown().await?;
    Ok(())
}

async fn delivered(deliveries: &CancelCellEvidence, count: usize) -> TestResult {
    tokio::time::timeout(Duration::from_secs(10), async {
        while lock_mutex(&deliveries.deliveries).len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

fn nested(
    evidence: &FakeCompositionEvidence,
    deliveries: &Arc<CancelCellEvidence>,
    id: &str,
    name: &str,
    input: serde_json::Value,
) -> TestResult {
    lock_mutex(&evidence.events)
        .as_ref()
        .ok_or("events")?
        .send(LocalCompositionEvent::NestedCall {
            call: crate::local_composition::LocalCompositionNestedCall {
                cell_id: "C".into(),
                runtime_call_id: id.into(),
                tool_name: name.into(),
                input: Some(input),
            },
            responder: Box::new(CancelCellResponder {
                identity: id.into(),
                evidence: deliveries.clone(),
            }),
        })?;
    Ok(())
}

#[tokio::test]
async fn in_flight_nested_settlement_keeps_closed_debt_until_outer_boundary() -> TestResult {
    if std::env::var_os("PLAK_TEST_LEDGER_SETTLEMENT_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::in_flight_nested_settlement_keeps_closed_debt_until_outer_boundary"
        )
        .strip_prefix("plakous_agent::")
        .ok_or("test name")?;
        for control in ["none", "cancel", "shutdown"] {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", name, "--nocapture"])
                .env("PLAK_TEST_LEDGER_SETTLEMENT_CHILD", "1")
                .env("PLAK_TEST_LEDGER_SETTLEMENT_CONTROL", control)
                .env("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER", "16")
                .output()?;
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let (agent, evidence, store) = settlement_agent(directory.path()).await?;
    settlement_case(&agent, &evidence, &store).await?;
    agent.shutdown().await?;
    Ok(())
}

async fn settlement_agent(
    directory: &std::path::Path,
) -> TestResult<(Agent, Arc<FakeCompositionEvidence>, Arc<TestCreateStore>)> {
    let workspace = Arc::new(WorkspacePolicy::host(directory, ".")?);
    let registry = neutral_composition_registry_with_tools(
        false,
        vec![probe_registration(
            Arc::new(SessionPairs {
                calls: AtomicUsize::new(0),
                tools: std::array::from_fn(|_| {
                    Arc::new(SessionUpdateTool::new(
                        SessionUpdateFixtureMode::ActiveThenJoinedForeground,
                    ))
                }),
            }),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )?],
    )?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_aa00, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("emit many text items")?),
                (14, "exec", serde_json::to_string("seed job")?),
            ])
            .with_assistant_continuations(1..14),
    );
    let configured =
        neutral_composition_model(&model, descriptor.clone(), authority.clone(), &registry)?;
    let target = composition_projection_target(&configured, &descriptor)?;
    let composition = review_composition(registry.clone(), target, false)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = composition_test_builder(
        workspace,
        registry,
        &model,
        descriptor,
        authority,
        composition,
        (evidence.clone(), LocalCompositionLifecycle::RootTurnJoined),
    )?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create("create:settlement-retirement", builder)
        .await?;
    Ok((agent, evidence, store))
}

async fn settlement_case(
    agent: &Agent,
    evidence: &Arc<FakeCompositionEvidence>,
    store: &TestCreateStore,
) -> TestResult {
    let turn = agent.prompt("keep the cell active").await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while evidence.physical_cell_starts.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let before = agent.checkpoint();
    let debt = before
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
                    | DurableOperationKind::ToolBatchContinuation { .. }
            )
        })
        .map(DurableOperationRecord::operation_id)
        .collect::<Vec<_>>();
    assert!(debt.len() >= 2);
    let deliveries = Arc::new(CancelCellEvidence::default());
    for index in 0..4 {
        nested(
            evidence,
            &deliveries,
            &format!("start-{index}"),
            "probe",
            json!({}),
        )?;
        delivered(&deliveries, index + 1).await?;
    }
    let settled = agent.checkpoint();
    assert!(
        settled.manifest().ledger().operation_count() >= 24,
        "count={}, deliveries={:?}",
        settled.manifest().ledger().operation_count(),
        lock_mutex(&deliveries.deliveries)
    );
    assert_eq!(
        settled
            .manifest()
            .ledger()
            .operations()
            .iter()
            .filter(
                |record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                    && matches!(record.state(), DurableOperationState::Finalized(_))
            )
            .count(),
        2
    );
    let eligible = crate::runtime::operation_record::retire_live_ledger(
        settled.manifest().ledger(),
        turn.id(),
        &BTreeSet::from([turn.id()]),
    )?;
    assert!(
        debt.iter().any(|id| eligible.operation(*id).is_none()),
        "the fixture must contain reclaimable below-P debt above the emergency threshold"
    );
    for id in debt {
        assert!(
            settled.manifest().ledger().operation(id).is_some(),
            "nested settlement retired {id}"
        );
    }
    assert!(settled.manifest().ledger().retired_counter().is_none());
    match std::env::var("PLAK_TEST_LEDGER_SETTLEMENT_CONTROL").as_deref() {
        Ok("cancel") => {
            turn.control().cancel().await?;
        }
        Ok("shutdown") => {
            let submission = agent
                .command(TurnCommand::Shutdown {
                    command_id: CommandId::new("shutdown:in-flight-retirement")?,
                })
                .await?;
            assert!(submission.acknowledgement().is_accepted());
        }
        _ => {}
    }
    evidence.source_release.notify_one();
    let result = turn.result().await?;
    assert_control_retirement(&settled, &result, store)?;
    assert!(
        result
            .checkpoint()
            .manifest()
            .ledger()
            .retired_attempts(result.turn_id())
            > 0
    );
    assert!(lock_mutex(&store.accepted_commits).len() > 20);
    Ok(())
}

fn assert_control_retirement(
    before: &RuntimeCheckpoint,
    result: &TurnResult,
    store: &TestCreateStore,
) -> TestResult {
    let control = std::env::var("PLAK_TEST_LEDGER_SETTLEMENT_CONTROL")?;
    if control == "none" {
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        return Ok(());
    }
    assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
    let snapshots = lock_mutex(&store.accepted_commits);
    let receipt = snapshots
        .iter()
        .find_map(|(checkpoint, _)| {
            let ledger = checkpoint.manifest().ledger();
            ledger
                .operations()
                .iter()
                .any(|record| {
                    before
                        .manifest()
                        .ledger()
                        .operation(record.operation_id())
                        .is_none()
                        && matches!(record.kind(), DurableOperationKind::Command { .. })
                        && matches!(record.state(), DurableOperationState::Finalized(_))
                })
                .then_some(checkpoint)
        })
        .ok_or("control receipt publication")?;
    let ledger = receipt.manifest().ledger();
    for record in before.manifest().ledger().operations() {
        assert!(
            ledger.operation(record.operation_id()).is_some(),
            "{control} receipt retired {} while the cell was in flight",
            record.operation_id()
        );
    }
    assert_eq!(
        ledger.retired_counter(),
        before.manifest().ledger().retired_counter()
    );
    assert!(ledger.operations().iter().any(|record| matches!(
        record.kind(),
        DurableOperationKind::CompositionEvaluation { .. }
    ) && !matches!(
        record.state(),
        DurableOperationState::Finalized(_)
    )));
    let evaluation = before
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
            ) && !matches!(record.state(), DurableOperationState::Finalized(_))
        })
        .ok_or("in-flight evaluation")?
        .operation_id();
    let first_retirement = snapshots
        .iter()
        .find(|(checkpoint, _)| {
            checkpoint
                .manifest()
                .ledger()
                .retired_attempts(result.turn_id())
                > 0
        })
        .ok_or("outer-boundary retirement")?;
    assert!(
        matches!(
            first_retirement
                .0
                .manifest()
                .ledger()
                .operation(evaluation)
                .ok_or("retained evaluation at outer boundary")?
                .state(),
            DurableOperationState::Finalized(_)
        ),
        "{control} retired evidence before the outer boundary"
    );
    Ok(())
}
