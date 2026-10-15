use super::*;

/// Which shape of live cell the cancel-truthfulness fixtures drive.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum CancelCellMode {
    /// Execution startup has admitted source but not returned a cell.
    ExecutePending,
    /// A cell exists but its initial response is pending, without nested calls.
    InitialResponsePending,
    /// The cell never yields; two nested mutating commands run inside it.
    ExecInFlight,
    /// The cell yields at once; the model's `wait*` continuation parks.
    WaitWindow,
    /// The cell never yields; one real `exec_command` (`sleep 30`) runs
    /// inside it over the Codex-native catalog and process backend.
    ProcessInFlight,
    /// As `ProcessInFlight`, but the nested `exec_command` yields after
    /// 300 ms with the process still running, so the runtime records the
    /// live process session as a durable operation (the deepest poll
    /// chain the cell driver has).
    ProcessYield,
}

impl CancelCellMode {
    /// Whether the fixture drives the Codex-native catalog with the real
    /// process-session tools instead of the two probe tools.
    pub(super) const fn codex_process_catalog(self) -> bool {
        matches!(self, Self::ProcessInFlight | Self::ProcessYield)
    }

    /// The nested `exec_command` input for the process-backed modes.
    pub(super) fn process_input(self) -> Option<serde_json::Value> {
        match self {
            Self::ProcessInFlight => Some(json!({"cmd": "sleep 30", "yield_time_ms": 10_000})),
            Self::ProcessYield => Some(json!({"cmd": "sleep 30", "yield_time_ms": 300})),
            Self::ExecutePending
            | Self::InitialResponsePending
            | Self::ExecInFlight
            | Self::WaitWindow => None,
        }
    }
}

#[derive(Default)]
pub(super) struct CancelCellEvidence {
    pub(super) counters: CompositionCounters,
    pub(super) buffered_terminate_fixture: AtomicBool,
    /// The model opens a second cell on its second round.
    pub(super) second_exec_fixture: AtomicBool,
    pub(super) mixed_tool_fixture: AtomicBool,
    pub(super) top_level_process: Mutex<Option<Arc<SessionUpdateTool>>>,
    pub(super) steer_fixture: AtomicBool,
    pub(super) python_fixture: AtomicBool,
    pub(super) long_wait_fixture: AtomicBool,
    pub(super) nested_deadline_fixture: AtomicBool,
    pub(super) read_only_fixture: AtomicBool,
    pub(super) release_block: AtomicBool,
    pub(super) release_cell: AtomicBool,
    pub(super) wait_count: AtomicUsize,
    pub(super) execute_started: AtomicBool,
    /// Cells whose source this session admitted.
    pub(super) executes: AtomicUsize,
    pub(super) initial_response_started: AtomicBool,
    pub(super) done_delivered: AtomicBool,
    pub(super) block_started: AtomicBool,
    pub(super) block_joined: AtomicBool,
    pub(super) wait_started: AtomicBool,
    pub(super) shutdown_loss: Mutex<Option<String>>,
    pub(super) panic_cleanup_lifecycle: AtomicBool,
    pub(super) panic_terminate: AtomicBool,
    pub(super) terminal_poll: AtomicBool,
    pub(super) empty_poll: AtomicBool,
    pub(super) deliveries: Mutex<Vec<(String, bool, String)>>,
}

struct CancelCellTool {
    evidence: Arc<CancelCellEvidence>,
}

impl plakous_tools::ToolExecutor for CancelCellTool {
    fn execute(
        self: Arc<Self>,
        invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            match invocation.identity().name().as_str() {
                "nested_done" => {
                    ToolOutcome::success_text("{\"agent_id\":\"child-1\",\"status\":\"running\"}")
                }
                "nested_block" => {
                    let top_level = lock_mutex(&self.evidence.top_level_process).clone();
                    if let Some(tool) = top_level {
                        return tool.execute(invocation, context).await;
                    }
                    self.evidence.block_started.store(true, Ordering::Release);
                    if self.evidence.steer_fixture.load(Ordering::Acquire)
                        || self
                            .evidence
                            .nested_deadline_fixture
                            .load(Ordering::Acquire)
                    {
                        assert_eq!(
                            *context.preempt().borrow(),
                            plakous_tools::ToolPreempt::None
                        );
                        wait_for_scheduler_fact(&self.evidence.release_block).await;
                        assert!(!context.cancellation().is_cancelled());
                        assert_eq!(
                            *context.preempt().borrow(),
                            plakous_tools::ToolPreempt::None
                        );
                        self.evidence.block_joined.store(true, Ordering::Release);
                        return ToolOutcome::success_text("nested mutation settled once");
                    }
                    context.cancellation().cancelled().await;
                    self.evidence.block_joined.store(true, Ordering::Release);
                    Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Cancelled,
                        "nested command joined the cancel",
                    ))
                }
                other => Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    format!("unexpected cancel-cell tool {other}"),
                )),
            }
        })
    }
}

pub(super) struct CancelCellResponder {
    pub(super) identity: String,
    pub(super) evidence: Arc<CancelCellEvidence>,
}

impl crate::local_composition::LocalCompositionNestedResponder for CancelCellResponder {
    fn resolve(
        self: Box<Self>,
        value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            lock_mutex(&self.evidence.deliveries).push((
                self.identity.clone(),
                true,
                value.to_string(),
            ));
            if self.identity == "DONE" {
                self.evidence.done_delivered.store(true, Ordering::Release);
            }
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            lock_mutex(&self.evidence.deliveries).push((self.identity.clone(), false, reason));
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }
}

pub(super) struct CancelCellEvents {
    pub(super) next: usize,
    pub(super) mode: CancelCellMode,
    pub(super) evidence: Arc<CancelCellEvidence>,
}

impl CancelCellEvents {
    pub(super) fn nested(&self, identity: &str, tool_name: &str) -> LocalCompositionEvent {
        LocalCompositionEvent::NestedCall {
            call: crate::local_composition::LocalCompositionNestedCall {
                cell_id: "cancel-cell".to_owned(),
                runtime_call_id: identity.to_owned(),
                tool_name: tool_name.to_owned(),
                input: Some(json!({})),
            },
            responder: Box::new(CancelCellResponder {
                identity: identity.to_owned(),
                evidence: Arc::clone(&self.evidence),
            }),
        }
    }
}

pub(super) struct CancelCellSession {
    pub(super) mode: CancelCellMode,
    pub(super) evidence: Arc<CancelCellEvidence>,
}

pub(super) struct CancelCellIntegration {
    pub(super) descriptor: LocalCompositionDescriptor,
    pub(super) mode: CancelCellMode,
    pub(super) evidence: Arc<CancelCellEvidence>,
}

pub(super) fn cancel_cell_python_descriptor(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
) -> TestResult<LocalCompositionDescriptor> {
    let exact = PythonPtcV1::compile(
        registry,
        target,
        PythonPtcPresentationV1::StructuredFunctions,
        [
            ToolName::new("nested_done")?,
            ToolName::new("nested_block")?,
        ],
    )?;
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: exact
            .nested_tools()
            .iter()
            .map(|tool| LocalCompositionToolDefinition {
                name: tool.canonical_name().as_str().to_owned(),
                description: tool.description().to_owned(),
                input_schema: tool.composition_input_schema_value(),
            })
            .collect::<Vec<_>>()
            .into(),
        identity: exact.identity(),
        execute_name: "exec_py".into(),
        wait_name: "wait_py".into(),
        marker_executor: ToolExecutorId::new(
            plakous_tools::PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1,
        )?,
    })
}

pub(super) fn cancel_cell_descriptor(
    exact: &plakous_compose::ProductJavaScriptCompositionDescriptorV1,
) -> TestResult<LocalCompositionDescriptor> {
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
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
        execute_name: "exec_js".into(),
        wait_name: "wait_js".into(),
        marker_executor: ToolExecutorId::new(
            "plakous.product-javascript-composition.driver-marker.v1",
        )?,
    })
}

pub(super) fn extend_cancel_cell_outer(
    descriptor: &mut LocalCompositionDescriptor,
    registry: FrozenToolRegistry,
) -> TestResult {
    descriptor.outer_projection = plakous_tools::internal::projection_map(
        registry,
        descriptor.outer_projection.target().clone(),
        descriptor
            .outer_projection
            .entries()
            .iter()
            .chain(descriptor.nested_projection.entries())
            .cloned(),
    )?;
    descriptor.identity = Digest::sha256(
        DigestDomain::ToolDefinition,
        1,
        &[
            descriptor.outer_projection.digest().bytes(),
            descriptor.nested_projection.digest().bytes(),
        ],
    )?;
    Ok(())
}

fn cancel_cell_registry(evidence: Arc<CancelCellEvidence>) -> TestResult<FrozenToolRegistry> {
    let read_only = evidence.read_only_fixture.load(Ordering::Acquire);
    let executor: Arc<dyn plakous_tools::ToolExecutor> = Arc::new(CancelCellTool { evidence });
    let mut registry = ToolRegistryBuilder::default();
    for name in ["nested_done", "nested_block"] {
        let definition = ToolDefinition::new(
            ToolName::new(name)?,
            "1.0.0",
            format!("Cancel-cell probe {name}."),
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::model_content(128, 1_048_576)?,
            if read_only {
                ToolConcurrency::Parallel
            } else {
                ToolConcurrency::Serial
            },
            if read_only {
                ToolEffect::ReadOnly
            } else {
                ToolEffect::Mutating
            },
        )?;
        registry.register(
            ToolRegistration::new(
                RegisteredToolId::new(format!("cancel-cell.{name}.registration"))?,
                ToolExecutorId::new(format!("cancel-cell.{name}.executor"))?,
                plakous_tools::ToolExecutorBindingIdentity::new(
                    1,
                    &[format!("cancel-cell.{name}.executor").as_bytes()],
                )?,
                definition,
                Arc::clone(&executor),
            )
            .nested_eligible(),
        )?;
    }
    ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    PythonPtcV1::register_markers_into(&mut registry)?;
    Ok(registry.freeze())
}

/// Builds one agent whose model opens a cell at round 0 (and, in the wait
/// window shape, parks on `wait_js` at round 1) over the cancel-cell fixture.
pub(super) async fn cancel_cell_agent(
    mode: CancelCellMode,
    evidence: Arc<CancelCellEvidence>,
    directory: &std::path::Path,
) -> TestResult<Agent> {
    Ok(cancel_cell_agent_with_model(mode, evidence, directory)
        .await?
        .0)
}

async fn cancel_cell_agent_with_model(
    mode: CancelCellMode,
    evidence: Arc<CancelCellEvidence>,
    directory: &std::path::Path,
) -> TestResult<(Agent, Arc<ArtifactModel>)> {
    let (builder, model) = cancel_cell_builder(mode, evidence, directory)?;
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    Ok((sessions.create("create:cancel-cell", builder).await?, model))
}

pub(super) fn cancel_cell_builder(
    mode: CancelCellMode,
    evidence: Arc<CancelCellEvidence>,
    directory: &std::path::Path,
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    let workspace = Arc::new(WorkspacePolicy::host(directory, ".")?);
    let registry = if mode.codex_process_catalog() {
        neutral_composition_test_registry()?
    } else {
        cancel_cell_registry(Arc::clone(&evidence))?
    };
    let (execute_name, wait_name) = if evidence.python_fixture.load(Ordering::Acquire) {
        ("exec_py", "wait_py")
    } else if mode.codex_process_catalog() {
        ("exec", "wait")
    } else {
        ("exec_js", "wait_js")
    };
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    // The Codex-native `exec` marker is a custom tool whose argument is the
    // bare source string; the product JavaScript marker takes an object.
    let exec_arguments = if evidence.python_fixture.load(Ordering::Acquire) {
        serde_json::to_string(&json!({"code": "run until cancelled"}))?
    } else if mode.codex_process_catalog() {
        serde_json::to_string("run until cancelled")?
    } else {
        serde_json::to_string(&json!({"source": "run until cancelled"}))?
    };
    let mut scripted = vec![(0, execute_name, exec_arguments.clone())];
    if evidence.second_exec_fixture.load(Ordering::Acquire) {
        scripted.push((1, execute_name, exec_arguments));
    }
    if mode == CancelCellMode::WaitWindow {
        let mut arguments = json!({"cell_id": "cancel-cell", "yield_time_ms": if evidence.long_wait_fixture.load(Ordering::Acquire) { 600_000 } else { 10 }});
        if evidence.buffered_terminate_fixture.load(Ordering::Acquire) {
            arguments["terminate"] = json!(true);
        }
        scripted.push((1, wait_name, serde_json::to_string(&arguments)?));
    }
    if evidence.mixed_tool_fixture.load(Ordering::Acquire) {
        scripted.push((1, "nested_block", "{}".into()));
        scripted.push((2, "nested_done", "{}".into()));
    } else if evidence.steer_fixture.load(Ordering::Acquire) {
        scripted.push((
            if mode == CancelCellMode::WaitWindow {
                2
            } else {
                1
            },
            wait_name,
            serde_json::to_string(&json!({"cell_id": "cancel-cell", "yield_time_ms": 10}))?,
        ));
    }
    let mut fixture =
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_7000, false)
            .with_scripted_tool_calls(scripted);
    fixture.response_provenance = true;
    if evidence.mixed_tool_fixture.load(Ordering::Acquire)
        && evidence.read_only_fixture.load(Ordering::Acquire)
    {
        fixture = fixture.with_second_direct_tool_named("nested_done")?;
        fixture.secondary_tool_on = Some(1);
    }
    let model = Arc::new(fixture);
    let exposed = if evidence.mixed_tool_fixture.load(Ordering::Acquire) {
        vec![execute_name, wait_name, "nested_block", "nested_done"]
    } else {
        vec![execute_name, wait_name]
    };
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&exposed),
        if evidence.mixed_tool_fixture.load(Ordering::Acquire) {
            ToolPresentationMode::LocalComposition
        } else {
            ToolPresentationMode::LocalCompositionOnly
        },
        false,
    )?;
    let target = composition_projection_target(&configured, &descriptor)?;
    let mut composition_descriptor = if evidence.python_fixture.load(Ordering::Acquire) {
        cancel_cell_python_descriptor(registry.clone(), target)?
    } else if mode.codex_process_catalog() {
        neutral_composition_descriptor(registry.clone(), target)?
    } else {
        let exact = ProductJavaScriptCompositionV1::compile(
            registry.clone(),
            target,
            ["nested_done", "nested_block"]
                .into_iter()
                .map(ToolName::new)
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        cancel_cell_descriptor(&exact)?
    };
    if evidence.mixed_tool_fixture.load(Ordering::Acquire) {
        extend_cancel_cell_outer(&mut composition_descriptor, registry.clone())?;
    }
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(FakeComposition::Cancel(CancelCellIntegration {
            descriptor: composition_descriptor.clone(),
            mode,
            evidence,
        }));
    let builder = code_cell_agent_builder(
        model.as_ref(),
        configured,
        registry,
        workspace,
        "test.cancel-cell",
        composition_descriptor,
        integration,
    )?;
    Ok((builder, model))
}

pub(super) fn durable_state_name(state: &DurableOperationState) -> &'static str {
    match state {
        DurableOperationState::Prepared => "Prepared",
        DurableOperationState::DecisionRequired(_) => "DecisionRequired",
        DurableOperationState::ContinuationRequired(_) => "ContinuationRequired",
        DurableOperationState::Claimed(_) => "Claimed",
        DurableOperationState::Dispatched => "Dispatched",
        DurableOperationState::LocatorBound(_) => "LocatorBound",
        DurableOperationState::Completed(_) => "Completed",
        DurableOperationState::ProvenNotDispatched(_) => "ProvenNotDispatched",
        DurableOperationState::OutcomeUnknown(_) => "OutcomeUnknown",
        DurableOperationState::Finalized(_) => "Finalized",
    }
}

pub(super) fn describe_ledger(result: &TurnResult) -> String {
    result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .map(|record| {
            format!(
                "op {} {:?} -> {}",
                record.operation_id(),
                record.kind(),
                durable_state_name(record.state())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cancels the admitted root once `ready` reports the in-flight fact and
/// returns the terminal result together with its command identity.
async fn cancel_cell_turn(
    agent: &Agent,
    label: &str,
    ready: &AtomicBool,
) -> TestResult<(TurnResult, CommandId)> {
    let command_id = CommandId::new(format!("cancel-cell:{label}"))?;
    let turn = agent
        .start(command_id.clone(), "open a cell")
        .await?
        .into_turn()
        .ok_or("cancel-cell root was not admitted")?;
    let control = turn.control();
    wait_for_scheduler_fact(ready).await;
    assert_eq!(control.cancel().await?, CancellationStatus::Requested);
    Ok((turn.result().await?, command_id))
}

fn assert_cancelled_and_quiescent(result: &TurnResult, command_id: &CommandId) -> TestResult {
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
        "an observed cancel must terminalise Cancelled, got {:?}\n{}",
        result.outcome(),
        describe_ledger(result)
    );
    assert_exact_terminal_result(result, command_id)?;
    assert!(
        matches!(
            result.checkpoint().manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ),
        "a cancelled turn must leave a quiescent checkpoint\n{}",
        describe_ledger(result)
    );
    Ok(())
}

#[derive(Default)]
struct CancelledBatchTool {
    calls: AtomicUsize,
    second_started: AtomicBool,
    joined: AtomicBool,
}

impl plakous_tools::ToolExecutor for CancelledBatchTool {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                return ToolOutcome::success_text("completed effect evidence");
            }
            self.second_started.store(true, Ordering::Release);
            context.cancellation().cancelled().await;
            self.joined.store(true, Ordering::Release);
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "second call joined cancellation",
            ))
        })
    }
}

fn cancelled_batch_registry(tool: Arc<CancelledBatchTool>) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Record one bounded outcome.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool,
    ))?;
    Ok(registry.freeze())
}

#[tokio::test]
async fn cancelled_batch_keeps_completed_result_in_next_request_and_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        95_000,
        false,
    )
    .with_tool_first()
    .with_second_direct_tool()?;
    fixture.response_provenance = true;
    let model = Arc::new(fixture);
    let tool = Arc::new(CancelledBatchTool::default());
    let registry = cancelled_batch_registry(Arc::clone(&tool))?;
    let configured = configured_test_model(
        model.clone(),
        descriptor,
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let builder = AgentBuilder::new(configured, registry, Arc::clone(&workspace))
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    let agent = sessions.create("create:cancelled-batch", builder).await?;
    let command_id = CommandId::new("root:cancelled-batch")?;
    let turn = agent
        .start(command_id.clone(), "run both probes")
        .await?
        .into_turn()
        .ok_or("root was not admitted")?;
    let control = turn.control();
    wait_for_scheduler_fact(&tool.second_started).await;
    assert_eq!(control.cancel().await?, CancellationStatus::Requested);
    let cancelled = turn.result().await?;
    assert_cancelled_and_quiescent(&cancelled, &command_id)?;
    assert!(tool.joined.load(Ordering::Acquire));
    assert_eq!(tool.calls.load(Ordering::Acquire), 2);

    let assert_group = |plan: &ValidatedRequestPlan| {
        let results = plan
            .request()
            .input()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert!(
            matches!(results[0].parts(), [ToolResultPart::Text(text)] if text.text() == "completed effect evidence")
        );
        assert!(
            matches!(results[1].parts(), [ToolResultPart::Text(text)] if text.text().contains("cancelled"))
        );
    };

    let next = agent
        .prompt("continue with the known result")
        .await?
        .result()
        .await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    {
        let observed = lock_mutex(&model.observed_provenance);
        assert_group(&observed.last().ok_or("next request not prepared")?.0);
    }
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let reopened_model = Arc::new(ArtifactModel::new(
        model.descriptor.clone(),
        model.authority.clone(),
        0,
        96_000,
        false,
    ));
    let reopen_registry = cancelled_batch_registry(Arc::clone(&tool))?;
    let configured = configured_test_model(
        reopened_model.clone(),
        reopened_model.descriptor.clone(),
        reopened_model.authority.clone(),
        &reopen_registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let reopen_builder = AgentBuilder::new(configured, reopen_registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:cancelled-batch", address, reopen_builder)
        .await?
    else {
        return Err("cancelled batch should reopen without effect recovery".into());
    };
    assert_eq!(
        reopened
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .identity(),
        next.checkpoint()
            .manifest()
            .canonical()
            .history()
            .identity(),
    );
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn root_admission_dispatches_only_renew_and_commit_unless_reclaiming_pressure() -> TestResult
{
    for pressure in [false, true] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease duration")?,
        )?;
        let descriptor = durable_test_descriptor("test.route")?;
        let model = Arc::new(ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            95_000,
            false,
        ));
        let registry = ToolRegistryBuilder::default().freeze();
        let configured = configured_test_model(
            model.clone(),
            descriptor,
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        let builder = AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            );
        let agent = sessions.create("create:admission-budget", builder).await?;
        store.admission_usage.store(true, Ordering::Release);
        store
            .reclaim_admission_pressure
            .store(pressure, Ordering::Release);
        *lock_mutex(&store.ordinary_exhausted) = Some(Arc::new(AtomicBool::new(pressure)));
        // Capture through the admission commit, before subsequent provider publications.
        *lock_mutex(&store.admission_jobs) = Some(Vec::new());
        let turn = agent
            .start(CommandId::new("root:admission-budget")?, "hello")
            .await?
            .into_turn()
            .ok_or("turn")?;
        let expected = if pressure {
            vec!["reclaim", "renew", "commit"]
        } else {
            vec!["renew", "commit"]
        };
        assert_eq!(*lock_mutex(&store.captured_admission_jobs), expected);
        assert!(matches!(
            turn.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn precommit_store_rejections_keep_admission_open_without_ack_recovery() -> TestResult {
    for rejection in [
        SessionStoreError::StorageFull,
        SessionStoreError::Corrupt(crate::session::StoreIntegrityReason::MissingChild),
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease duration")?,
        )?;
        let descriptor = durable_test_descriptor("test.route")?;
        let model = Arc::new(ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            95_000,
            false,
        ));
        let registry = ToolRegistryBuilder::default().freeze();
        let configured = configured_test_model(
            model.clone(),
            descriptor,
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        let builder = AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            );
        let agent = sessions
            .create("create:definite-rejection", builder)
            .await?;
        let head = agent.checkpoint().id();
        let recoveries = store.recover_commit_calls.load(Ordering::Acquire);
        *lock_mutex(&store.reject_next_commit) = Some(rejection);
        let command_id = CommandId::new("root:definite-rejection")?;
        let submission = agent
            .command(TurnCommand::Start {
                command_id: command_id.clone(),
                text: "not admitted".into(),
            })
            .await?;
        assert_eq!(
            submission
                .acknowledgement()
                .rejection()
                .ok_or("rejection")?
                .kind(),
            ControlErrorKind::AdmissionRejected
        );
        assert!(
            submission
                .acknowledgement()
                .rejection()
                .ok_or("rejection")?
                .message()
                .contains("prompt was not admitted")
        );
        assert_eq!(agent.checkpoint().id(), head);
        assert_eq!(
            store.recover_commit_calls.load(Ordering::Acquire),
            recoveries
        );
        assert!(
            !agent
                .checkpoint()
                .manifest()
                .ledger()
                .commands()
                .iter()
                .any(|record| record.command_id() == command_id.as_str())
        );
        // The same exact identity is still unrecorded and may now be admitted.
        let turn = agent
            .start(command_id, "not admitted")
            .await?
            .into_turn()
            .ok_or("turn")?;
        assert!(matches!(
            turn.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn ambiguous_root_admission_closes_admission_and_same_id_cannot_run_twice() -> TestResult {
    for committed in [false, true] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease duration")?,
        )?;
        let descriptor = durable_test_descriptor("test.route")?;
        let model = Arc::new(ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            95_000,
            false,
        ));
        let registry = ToolRegistryBuilder::default().freeze();
        let configured = configured_test_model(
            model.clone(),
            descriptor,
            model.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        let builder = AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            );
        let agent = sessions
            .create("create:definite-rejection", builder)
            .await?;
        let recoveries = store.recover_commit_calls.load(Ordering::Acquire);
        store
            .force_recover_commit_not_found
            .store(true, Ordering::Release);
        if committed {
            store.lose_commit_ack_on(store.commit_calls.load(Ordering::Acquire) + 1);
        } else {
            *lock_mutex(&store.reject_next_commit) = Some(SessionStoreError::BackendUnavailable);
            store.commits_halted.store(true, Ordering::Release);
        }
        let command_id = CommandId::new("root:ambiguous-admission")?;
        let first = agent
            .command(TurnCommand::Start {
                command_id: command_id.clone(),
                text: "exact ambiguous prompt".into(),
            })
            .await?;
        assert_eq!(
            first
                .acknowledgement()
                .rejection()
                .ok_or("lost acknowledgement")?
                .kind(),
            ControlErrorKind::OutcomeUnknown
        );
        assert!(!first.acknowledgement().is_accepted());
        assert!(first.into_turn().is_none());
        assert!(store.recover_commit_calls.load(Ordering::Acquire) > recoveries);
        for id in [command_id.clone(), CommandId::new("root:after-unknown")?] {
            let retry = agent
                .command(TurnCommand::Start {
                    command_id: id,
                    text: "exact ambiguous prompt".into(),
                })
                .await?;
            assert!(!retry.acknowledgement().is_accepted());
            assert!(retry.into_turn().is_none());
        }
        assert_eq!(model.calls.load(Ordering::Acquire), 0);
        {
            let state = lock_mutex(&store.state);
            let durable_commands = state
                .stored
                .as_ref()
                .ok_or("durable checkpoint")?
                .checkpoint()
                .manifest()
                .ledger()
                .commands()
                .into_iter()
                .filter(|record| record.command_id() == command_id.as_str())
                .count();
            assert_eq!(durable_commands, usize::from(committed));
        }
        let _ = agent.shutdown().await;
    }
    Ok(())
}

#[derive(Default)]
struct CountingMutationTool {
    calls: AtomicUsize,
}

impl plakous_tools::ToolExecutor for CountingMutationTool {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            ToolOutcome::success_text("completed effect evidence")
        })
    }
}

fn counting_mutation_registry(tool: Arc<CountingMutationTool>) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Apply one bounded mutation.",
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
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("mutation-probe.registration")?,
        ToolExecutorId::new("mutation-probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"mutation-probe.executor"])?,
        definition,
        tool,
    ))?;
    Ok(registry.freeze())
}

pub(super) fn text_of(item: &ConversationItem) -> String {
    item.blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect()
}

fn counting_probe_registry(
    tool: Arc<CountingMutationTool>,
    effect: ToolEffect,
    concurrency: ToolConcurrency,
) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Probe one bounded value.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        concurrency,
        effect,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("counting-probe.registration")?,
        ToolExecutorId::new("counting-probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"counting-probe.executor"])?,
        definition,
        tool,
    ))?;
    Ok(registry.freeze())
}

/// Refuses each ordinary commit of a two-call tool turn in turn: the tool
/// prepares, the mutating claims, the read-only group dispatch and the model
/// fences. Every refusal is definite: the turn publishes `Failed` of kind
/// `Session` (or the root is refused for storage pressure), never
/// `recovery_required`, and every tool that ran has its recorded result.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one refusal campaign over both tool effect classes"
)]
async fn an_ordinary_refusal_at_any_tool_fence_fails_the_turn_definitely() -> TestResult {
    for (effect, concurrency) in [
        (ToolEffect::ReadOnly, ToolConcurrency::Parallel),
        (ToolEffect::Mutating, ToolConcurrency::Serial),
    ] {
        let mut refused_turns = 0_usize;
        for refuse_on in 1_usize..64 {
            let directory = tempfile::tempdir()?;
            let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
            let store = Arc::new(TestCreateStore::default());
            store
                .refuse_ordinary_commit_on
                .store(refuse_on, Ordering::Release);
            let sessions = DurableSessionRuntime::current(
                store.clone(),
                RuntimeDurability::Required,
                NonZeroU64::new(30).ok_or("nonzero lease")?,
            )?;
            let descriptor = durable_test_descriptor("test.route")?;
            let model = Arc::new(
                ArtifactModel::new(
                    descriptor.clone(),
                    artifact_authority(&descriptor, 1)?,
                    0,
                    95_500,
                    false,
                )
                .with_tool_first()
                .with_second_direct_tool()?,
            );
            let tool = Arc::new(CountingMutationTool::default());
            let registry = counting_probe_registry(Arc::clone(&tool), effect, concurrency.clone())?;
            let configured = configured_test_model(
                model.clone(),
                descriptor,
                model.authority.clone(),
                &registry,
                LocalDurabilityDisposition::DurableRequired,
                ContextAdmissionRequirement::AnyBounded,
            )?;
            let builder = AgentBuilder::new(configured, registry, workspace)
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                );
            let agent = sessions.create("create:refusal-campaign", builder).await?;
            let command_id = CommandId::new("root:refusal-campaign")?;
            let submission = agent
                .command(TurnCommand::Start {
                    command_id: command_id.clone(),
                    text: "use both probes".to_owned(),
                })
                .await?;
            if let Some(rejection) = submission.acknowledgement().rejection() {
                assert_eq!(rejection.kind(), ControlErrorKind::StoragePressure);
                assert_eq!(store.ordinary_refusals.load(Ordering::Acquire), 1);
                assert_eq!(tool.calls.load(Ordering::Acquire), 0);
                agent.shutdown().await?;
                continue;
            }
            let turn = submission
                .into_turn()
                .ok_or("an admitted root carried no turn")?;
            let result = turn.result().await?;
            if store.ordinary_refusals.load(Ordering::Acquire) == 0 {
                assert!(
                    matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                    "{effect:?} unrefused turn: {:?}",
                    result.outcome()
                );
                assert_eq!(tool.calls.load(Ordering::Acquire), 2);
                agent.shutdown().await?;
                break;
            }
            refused_turns += 1;
            let TurnResultOutcome::Failed(error) = result.outcome() else {
                return Err(format!(
                    "{effect:?} refusal of ordinary commit {refuse_on} ended {:?}\n{}",
                    result.outcome(),
                    describe_ledger(&result)
                )
                .into());
            };
            assert_eq!(error.kind(), AgentErrorKind::Session, "{refuse_on}");
            assert_exact_terminal_result(&result, &command_id)?;
            assert!(matches!(
                result.checkpoint().manifest().lifecycle(),
                RuntimeLifecycle::Quiescent
            ));
            let recorded_successes = result
                .conversation()
                .items()
                .iter()
                .flat_map(ConversationItem::blocks)
                .filter(|block| {
                    matches!(block, ContentBlock::ToolResult(result)
                        if result.kind() == ToolResultKind::Success)
                })
                .count();
            assert_eq!(
                tool.calls.load(Ordering::Acquire),
                recorded_successes,
                "{effect:?} refusal of ordinary commit {refuse_on} ran a tool it did not record"
            );
            agent.shutdown().await?;
        }
        assert!(
            refused_turns >= 4,
            "{effect:?} campaign refused only {refused_turns} running turns"
        );
    }
    Ok(())
}

/// An operational failure after one mutation completed still closes the batch:
/// the completed result and a typed not-dispatched result commit with their
/// calls, a failure note follows, and the next turn's model sees all of it.
#[tokio::test]
async fn failed_batch_commits_completed_effect_before_failing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store
        .fail_second_effect_preparation
        .store(true, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        95_050,
        false,
    )
    .with_tool_first()
    .with_second_direct_tool()?;
    fixture.response_provenance = true;
    let model = Arc::new(fixture);
    let tool = Arc::new(CountingMutationTool::default());
    let registry = counting_mutation_registry(Arc::clone(&tool))?;
    let configured = configured_test_model(
        model.clone(),
        descriptor,
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    let agent = sessions.create("create:failed-batch", builder).await?;
    let command_id = CommandId::new("root:failed-batch")?;
    let failed = agent
        .start(command_id.clone(), "apply both mutations")
        .await?
        .into_turn()
        .ok_or("root was not admitted")?
        .result()
        .await?;
    assert!(
        matches!(failed.outcome(), TurnResultOutcome::Failed(_)),
        "{:?}\n{}",
        failed.outcome(),
        describe_ledger(&failed)
    );
    assert_exact_terminal_result(&failed, &command_id)?;
    assert!(matches!(
        failed.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(tool.calls.load(Ordering::Acquire), 1);
    assert!(failed.context_overflow_remedy().is_none());

    let assert_closed = |items: &[ConversationItem]| {
        let calls = items
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
            .count();
        let results = items
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, 2);
        assert_eq!(results.len(), 2);
        assert!(
            matches!(results[0].parts(), [ToolResultPart::Text(text)] if text.text() == "completed effect evidence")
        );
        assert_eq!(results[1].kind(), ToolResultKind::Failure);
        assert!(
            matches!(results[1].parts(), [ToolResultPart::Text(text)]
                if text.text().contains("not dispatched because the turn failed")),
            "{:?}",
            results[1]
        );
        let note = items.last().expect("failure note");
        assert_eq!(note.actor(), Actor::User);
        assert_eq!(note.provenance(), Provenance::Instruction);
        assert!(text_of(note).contains("failed (Session) before it was answered"));
    };
    assert_closed(&failed.conversation().items().to_vec());

    let next = agent
        .prompt("what happened to the mutations?")
        .await?
        .result()
        .await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    {
        let observed = lock_mutex(&model.observed_provenance);
        let request = observed.last().ok_or("next request not prepared")?;
        let mut items = request.0.request().input().items().to_vec();
        assert_eq!(
            items.pop().map(|item| text_of(&item)).as_deref(),
            Some("what happened to the mutations?")
        );
        assert_closed(&items);
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_programmatic_batch_with_completed_effect_requires_recovery() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            95_500,
            false,
        )
        .with_tool_first()
        .with_mixed_programmatic_and_direct_tools()?,
    );
    let tool = Arc::new(CancelledBatchTool::default());
    let registry = cancelled_batch_registry(Arc::clone(&tool))?;
    let configured = configured_test_model(
        model.clone(),
        descriptor,
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = sessions
        .create(
            "create:cancelled-programmatic-batch",
            AgentBuilder::new(configured, registry, workspace)
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                ),
        )
        .await?;
    let turn = agent.prompt("run provider and direct probes").await?;
    let control = turn.control();
    wait_for_scheduler_fact(&tool.second_started).await;
    assert_eq!(control.cancel().await?, CancellationStatus::Requested);
    let result = turn.result().await?;
    assert!(tool.joined.load(Ordering::Acquire));
    assert_eq!(tool.calls.load(Ordering::Acquire), 2);
    assert!(
        matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
        "programmatic continuation must not close as a clean cancellation: {:?}",
        result.outcome()
    );
    assert!(agent.prompt("continue").await.is_err());
    agent.shutdown().await?;
    Ok(())
}

#[derive(Default)]
struct CancelledMixedBatchTool {
    read_started: AtomicBool,
    mutation_started: AtomicBool,
}

struct MutatorDecisionPolicy(ToolPolicyRevision);

impl MutatorDecisionPolicy {
    fn new() -> TestResult<Self> {
        Ok(Self(ToolPolicyRevision::new("test.mutator-decision", 1)?))
    }
}

impl ToolPolicy for MutatorDecisionPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        self.0.clone()
    }

    fn evaluate(&self, request: ToolPolicyRequest) -> ToolPolicyFuture {
        Box::pin(async move {
            if request.invocation().identity().name().as_str() == "mutator" {
                Ok(ToolPolicyDecision::DecisionRequired(
                    EffectDecisionPrompt::new("restore mutator decision"),
                ))
            } else {
                Ok(ToolPolicyDecision::Allowed)
            }
        })
    }
}

impl plakous_tools::ToolExecutor for CancelledMixedBatchTool {
    fn execute(
        self: Arc<Self>,
        invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            if invocation.identity().name().as_str() == "probe" {
                self.read_started.store(true, Ordering::Release);
                return ToolOutcome::success_text("read completed before restored decision");
            }
            self.mutation_started.store(true, Ordering::Release);
            context.cancellation().cancelled().await;
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "mutator joined cancellation",
            ))
        })
    }
}

fn cancelled_mixed_registry(tool: &Arc<CancelledMixedBatchTool>) -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    for (name, effect) in [
        ("mutator", ToolEffect::Mutating),
        ("probe", ToolEffect::ReadOnly),
    ] {
        let definition = ToolDefinition::new(
            ToolName::new(name)?,
            "1.0.0",
            "Exercise one cancellation slot.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::model_content(128, 1_048_576)?,
            ToolConcurrency::Serial,
            effect,
        )?;
        registry.register(ToolRegistration::new(
            RegisteredToolId::new(format!("cancel-mixed.{name}.registration"))?,
            ToolExecutorId::new(format!("cancel-mixed.{name}.executor"))?,
            plakous_tools::ToolExecutorBindingIdentity::new(
                1,
                &[format!("cancel-mixed.{name}.executor").as_bytes()],
            )?,
            definition,
            tool.clone(),
        ))?;
    }
    Ok(registry.freeze())
}

#[tokio::test]
async fn cancelled_mutation_proves_buffered_read_was_not_dispatched() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            95_800,
            false,
        )
        .with_tool_first()
        .with_second_direct_tool_named("mutator")?,
    );
    let tool = Arc::new(CancelledMixedBatchTool::default());
    let registry = cancelled_mixed_registry(&tool)?;
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor,
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        None,
        ToolPresentationMode::Direct,
        false,
    )?;
    let profile = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("test.cancelled-mixed-batch")?)
        .semantic_version(ProfileSemanticVersion::new(1, 0, 0))
        .provenance(HarnessProfileProvenance::Synthetic)
        .base_instructions("")
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
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
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:cancelled-mixed-batch",
            AgentBuilder::new(configured, registry, workspace)
                .harness_plan(plan)
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                ),
        )
        .await?;
    let command = CommandId::new("root:cancelled-mixed-batch")?;
    let turn = agent
        .start(command.clone(), "read then mutate")
        .await?
        .into_turn()
        .ok_or("root was not admitted")?;
    let control = turn.control();
    wait_for_scheduler_fact(&tool.mutation_started).await;
    assert_eq!(control.cancel().await?, CancellationStatus::Requested);
    let result = turn.result().await?;
    assert_cancelled_and_quiescent(&result, &command)?;
    assert!(!tool.read_started.load(Ordering::Acquire));
    assert!(
        result
            .conversation()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::ToolResult(result)
            if result.parts().iter().any(|part| matches!(part,
                ToolResultPart::Text(text) if text.text().contains("not dispatched")))))
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_restored_decision_keeps_completed_prefix_in_next_request() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        95_900,
        false,
    )
    .with_tool_first()
    .with_second_direct_tool_named("mutator")?;
    fixture.response_provenance = true;
    let model = Arc::new(fixture);
    let tool = Arc::new(CancelledMixedBatchTool::default());
    let registry = cancelled_mixed_registry(&tool)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:cancelled-restored-decision",
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?
                .tool_policy(Arc::new(MutatorDecisionPolicy::new()?)),
        )
        .await?;
    let command = CommandId::new("root:cancelled-restored-decision")?;
    let turn = original
        .start(command.clone(), "read then request mutation")
        .await?
        .into_turn()
        .ok_or("root was not admitted")?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    assert!(tool.read_started.load(Ordering::Acquire));
    assert!(!tool.mutation_started.load(Ordering::Acquire));
    crate::recovery_scope::require_recoverable(&original.checkpoint())?;
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking {
        agent,
        turn: restored,
    } = sessions
        .open(
            "open:cancelled-restored-decision",
            address,
            durable_decision_builder(workspace, Arc::clone(&model), registry)?
                .tool_policy(Arc::new(MutatorDecisionPolicy::new()?)),
        )
        .await?
    else {
        return Err("effect decision was not restored".into());
    };
    assert_eq!(restored.turn_id(), turn.id());
    let decision = restored
        .pending_decision()
        .ok_or("restored effect decision was absent")?
        .clone();
    assert_eq!(
        restored
            .control()
            .allow_effect(
                decision,
                EffectDecisionAuditId::new("test:cancel-restored")?
            )
            .await?,
        EffectDecisionStatus::Allowed
    );
    wait_for_scheduler_fact(&tool.mutation_started).await;
    assert_eq!(
        restored.control().cancel().await?,
        CancellationStatus::Requested
    );
    let result = restored.result().result().await?;
    assert_cancelled_and_quiescent(&result, &command)?;
    assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks)
        .any(|block| matches!(block, ContentBlock::ToolResult(tool_result)
            if tool_result.parts().iter().any(|part| matches!(part,
                ToolResultPart::Text(text) if text.text().contains("read completed before restored decision"))))));
    let next = agent
        .prompt("continue after restored cancellation")
        .await?
        .result()
        .await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    {
        let observed = lock_mutex(&model.observed_provenance);
        let (next_plan, _) = observed.last().ok_or("next prompt had no model request")?;
        assert!(next_plan.request().input().items().iter().flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::ToolResult(tool_result)
                if tool_result.parts().iter().any(|part| matches!(part,
                    ToolResultPart::Text(text) if text.text().contains("read completed before restored decision"))))));
    }
    agent.shutdown().await?;
    let _ = original.shutdown().await;
    Ok(())
}

/// Runs one cancel-cell scenario on a thread with an 8 MiB stack.
///
/// A cancel that lands inside a nested mutating effect is joined at the
/// bottom of the cell driver chain (turn loop, tool batch, outer marker,
/// cell, serial nested dispatch, restored effect, control handling). The
/// unoptimized poll frames of that chain sum past the 2 MiB default
/// test-thread stack; the release driver polls the same chain in a fraction
/// of it.
pub(super) fn run_cancel_cell_scenario<F, Fut>(scenario: F) -> TestResult
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = TestResult>,
{
    run_cancel_cell_scenario_on_stack(8 << 20, scenario)
}

fn run_cancel_cell_scenario_on_stack<F, Fut>(stack_bytes: usize, scenario: F) -> TestResult
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = TestResult>,
{
    run_cancel_cell_scenario_with_timeout(stack_bytes, Duration::from_secs(10), scenario)
}

pub(super) fn run_cancel_cell_scenario_with_timeout<F, Fut>(
    stack_bytes: usize,
    timeout: Duration,
    scenario: F,
) -> TestResult
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = TestResult>,
{
    let joined = std::thread::Builder::new()
        .name("cancel-cell-scenario".into())
        .stack_size(stack_bytes)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime
                .block_on(async { tokio::time::timeout(timeout, scenario()).await })
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())
        })?
        .join()
        .map_err(|_| "cancel-cell scenario thread panicked")?;
    joined.map_err(Into::into)
}

#[test]
fn terminate_panic_still_attempts_owned_composition_shutdown() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.panic_terminate.store(true, Ordering::Release);
        let agent = cancel_cell_agent(
            CancelCellMode::ExecInFlight,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let (result, _) =
            cancel_cell_turn(&agent, "terminate-panic", &evidence.block_started).await?;
        let TurnResultOutcome::RecoveryRequired(error) = result.outcome() else {
            panic!(
                "termination panic must require recovery: {:?}",
                result.outcome()
            );
        };
        assert!(error.message().contains("panicked"), "{}", error.message());
        let _ = agent.shutdown().await;
        assert!(evidence.counters.terminates.load(Ordering::Acquire) > 0);
        assert!(
            evidence.counters.session_shutdowns.load(Ordering::Acquire) > 0,
            "termination panic skipped owned composition shutdown"
        );
        Ok(())
    })
}

#[test]
fn cancel_during_a_cell_with_one_completed_and_one_running_nested_command_is_cancelled()
-> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        let (agent, model) = cancel_cell_agent_with_model(
            CancelCellMode::ExecInFlight,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let (result, command_id) =
            cancel_cell_turn(&agent, "exec", &evidence.block_started).await?;
        assert_cancelled_and_quiescent(&result, &command_id)?;
        assert!(
            result
                .conversation()
                .items()
                .iter()
                .flat_map(ConversationItem::blocks)
                .any(|block| matches!(block, ContentBlock::ToolResult(tool)
                if tool.parts().iter().any(|part| matches!(part,
                    ToolResultPart::Text(text) if text.text().contains("child-1"))))),
            "completed nested spawn receipt must remain in canonical history after cancelling its cell"
        );
        assert!(evidence.block_joined.load(Ordering::Acquire));
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
        assert_eq!(
            evidence.counters.session_shutdowns.load(Ordering::Acquire),
            1
        );
        {
            let deliveries = lock_mutex(&evidence.deliveries);
            assert!(
                deliveries.iter().any(|(id, ok, _)| id == "DONE" && *ok),
                "the completed nested command keeps its delivered result: {deliveries:?}"
            );
            assert!(
                deliveries.iter().any(|(id, ok, _)| id == "BLOCK" && !*ok),
                "the running nested command is rejected, never resolved: {deliveries:?}"
            );
        }
        let next = agent
            .prompt("continue after the cancel")
            .await?
            .result()
            .await?;
        assert!(
            matches!(next.outcome(), TurnResultOutcome::Completed(_)),
            "the session must continue after a cancelled cell, got {:?}",
            next.outcome()
        );
        {
            let observed = lock_mutex(&model.observed_provenance);
            let (next_plan, _) = observed.last().ok_or("next prompt had no model request")?;
            assert!(
                next_plan
                    .request()
                    .input()
                    .items()
                    .iter()
                    .flat_map(ConversationItem::blocks)
                    .any(|block| matches!(block, ContentBlock::ToolResult(tool)
                    if tool.parts().iter().any(|part| matches!(part,
                        ToolResultPart::Text(text) if text.text().contains("child-1"))))),
                "next model request lost the completed nested spawn receipt"
            );
        }
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn shutdown_during_composition_start_initial_response_and_wait_is_joined() -> TestResult {
    run_cancel_cell_scenario(|| async {
        for mode in [
            CancelCellMode::ExecutePending,
            CancelCellMode::InitialResponsePending,
            CancelCellMode::WaitWindow,
        ] {
            let directory = tempfile::tempdir()?;
            let evidence = Arc::new(CancelCellEvidence::default());
            let agent = cancel_cell_agent(mode, Arc::clone(&evidence), directory.path()).await?;
            let command_id = CommandId::new("shutdown-cell")?;
            let turn = agent
                .start(command_id.clone(), "open a cell")
                .await?
                .into_turn()
                .ok_or("shutdown-cell root was not admitted")?;
            let ready = match mode {
                CancelCellMode::ExecutePending => &evidence.execute_started,
                CancelCellMode::InitialResponsePending => &evidence.initial_response_started,
                _ => &evidence.wait_started,
            };
            wait_for_scheduler_fact(ready).await;
            agent.shutdown().await?;
            let result = turn.result().await?;
            assert_cancelled_and_quiescent(&result, &command_id)?;
            assert_eq!(
                evidence.counters.terminates.load(Ordering::Acquire),
                usize::from(mode != CancelCellMode::ExecutePending)
            );
            assert_eq!(
                evidence.counters.session_shutdowns.load(Ordering::Acquire),
                1
            );
            agent.shutdown().await?;
            assert!(agent.prompt("after shutdown").await.is_err());
        }
        Ok(())
    })
}

#[test]
fn cancel_during_the_wait_window_is_cancelled() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        let agent = cancel_cell_agent(
            CancelCellMode::WaitWindow,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let (result, command_id) = cancel_cell_turn(&agent, "wait", &evidence.wait_started).await?;
        assert_cancelled_and_quiescent(&result, &command_id)?;
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
        assert_eq!(
            evidence.counters.session_shutdowns.load(Ordering::Acquire),
            1
        );
        let next = agent
            .prompt("continue after the cancel")
            .await?
            .result()
            .await?;
        assert!(
            matches!(next.outcome(), TurnResultOutcome::Completed(_)),
            "the session must continue after a cancelled wait, got {:?}",
            next.outcome()
        );
        agent.shutdown().await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}

#[test]
fn cancel_whose_sidecar_never_acknowledges_shutdown_requires_recovery_naming_it() -> TestResult {
    const LOSS: &str = "plakous-js-host sidecar did not acknowledge shutdown within 2s";
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        *lock_mutex(&evidence.shutdown_loss) = Some(LOSS.to_owned());
        let agent = cancel_cell_agent(
            CancelCellMode::ExecInFlight,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let (result, _) = cancel_cell_turn(&agent, "lost", &evidence.block_started).await?;
        let TurnResultOutcome::RecoveryRequired(error) = result.outcome() else {
            panic!(
                "an unacknowledged sidecar is an unknown outcome, got {:?}",
                result.outcome()
            );
        };
        assert!(
            error.message().contains(LOSS),
            "recovery must name the unresponsive sidecar: {}",
            error.message()
        );
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn local_cleanup_panic_still_joins_owned_process_lifetimes() -> TestResult {
    run_cancel_cell_scenario_on_stack(RUNTIME_EXECUTOR_STACK_BYTES, || async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        let agent = cancel_cell_agent(
            CancelCellMode::ProcessYield,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let command_id = CommandId::new("cancel-cell:process-yield")?;
        let turn = agent
            .start(command_id.clone(), "open a cell")
            .await?
            .into_turn()
            .ok_or("cancel-cell root was not admitted")?;
        let control = turn.control();
        // The yielded process session reaches the cell as the nested
        // call's delivery; that is the observable end of the chain above.
        let delivered = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if lock_mutex(&evidence.deliveries)
                    .iter()
                    .any(|(identity, accepted, _)| identity == "SLEEP" && *accepted)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            delivered.is_ok(),
            "the yielded exec_command was never delivered to the cell: {:?}",
            lock_mutex(&evidence.deliveries)
        );
        evidence
            .panic_cleanup_lifecycle
            .store(true, Ordering::Release);
        assert_eq!(control.cancel().await?, CancellationStatus::Requested);
        let result = tokio::time::timeout(Duration::from_secs(5), turn.result()).await??;
        let TurnResultOutcome::RecoveryRequired(error) = result.outcome() else {
            panic!(
                "cleanup panic must require recovery: {:?}",
                result.outcome()
            );
        };
        assert!(error.message().contains("panicked"), "{}", error.message());
        assert!(error.message().contains("Cancelled"), "{}", error.message());
        let _ = agent.shutdown().await;
        assert_ne!(agent.checkpoint().manifest().jobs().rows(), []);
        assert!(
            agent
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .iter()
                .all(|record| {
                    !matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                        || !matches!(record.state(), DurableOperationState::Dispatched)
                }),
            "local cleanup panic skipped joined process lifetime retirement"
        );
        Ok(())
    })
}

/// A child whose shell
/// tool ran `sleep 40` aborted the whole host with `thread 'tokio-rt-worker'
/// has overflowed its stack`, 4/4, with no interrupt or close involved;
/// `sleep 8` was fine. A command that outlives its yield window comes back
/// as a live process session, and recording that session as a durable
/// operation (`apply_process_session_update` → `prepare_operation` →
/// `publish_checkpoint` → bundle codec) is polled at the bottom of the
/// cell driver chain: 2.09 MiB of unoptimized poll frames, measured, on a
/// 2 MiB worker. The chain must fit the stack hosts are told to give
/// their executor threads, [`RUNTIME_EXECUTOR_STACK_BYTES`]; on the
/// `std::thread` default this scenario aborts the process.
#[test]
fn a_nested_process_command_that_outlives_its_yield_window_fits_the_executor_stack() -> TestResult {
    run_cancel_cell_scenario_on_stack(RUNTIME_EXECUTOR_STACK_BYTES, || async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        let agent = cancel_cell_agent(
            CancelCellMode::ProcessYield,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let command_id = CommandId::new("cancel-cell:process-yield")?;
        let turn = agent
            .start(command_id.clone(), "open a cell")
            .await?
            .into_turn()
            .ok_or("cancel-cell root was not admitted")?;
        let control = turn.control();
        // The yielded process session reaches the cell as the nested
        // call's delivery; that is the observable end of the chain above.
        let delivered = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if lock_mutex(&evidence.deliveries)
                    .iter()
                    .any(|(identity, accepted, _)| identity == "SLEEP" && *accepted)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            delivered.is_ok(),
            "the yielded exec_command was never delivered to the cell: {:?}",
            lock_mutex(&evidence.deliveries)
        );
        assert_eq!(control.cancel().await?, CancellationStatus::Requested);
        let result = turn.result().await?;
        assert_cancelled_and_quiescent(&result, &command_id)?;
        agent.shutdown().await?;
        Ok(())
    })
}

#[test]
fn nested_terminal_read_does_not_consume_the_job_notice() -> TestResult {
    run_cancel_cell_scenario_on_stack(RUNTIME_EXECUTOR_STACK_BYTES, || async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.terminal_poll.store(true, Ordering::Release);
        let agent = cancel_cell_agent(
            CancelCellMode::ProcessYield,
            evidence.clone(),
            directory.path(),
        )
        .await?;
        let turn = agent.prompt("start and read a job inside a cell").await?;
        let control = turn.control();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if lock_mutex(&evidence.deliveries)
                    .iter()
                    .any(|(id, accepted, _)| id == "TERMINAL" && *accepted)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        control.cancel().await?;
        let result = turn.result().await?;
        let rows = result.checkpoint().manifest().jobs().rows();
        assert_eq!(
            rows.len(),
            1,
            "a nested read consumed a model-invisible terminal result"
        );
        assert_eq!(rows[0].number, 1);
        assert!(
            result
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .iter()
                .all(|record| {
                    !matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                        || matches!(record.state(), DurableOperationState::Finalized(_))
                }),
            "the enclosing close did not commit promotion"
        );
        agent.shutdown().await?;
        Ok(())
    })
}

/// Esc during a cell whose nested
/// `exec_command` was inside its 10 s yield window settled only when that
/// window ran out, past the host's five-second cancel settle. A running
/// nested command must join the cancel promptly, not at its next deadline.
#[test]
fn cancel_during_a_cell_running_a_real_process_command_joins_inside_the_settle_window() -> TestResult
{
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        let agent = cancel_cell_agent(
            CancelCellMode::ProcessInFlight,
            Arc::clone(&evidence),
            directory.path(),
        )
        .await?;
        let command_id = CommandId::new("cancel-cell:process")?;
        let mut turn = agent
            .start(command_id.clone(), "open a cell")
            .await?
            .into_turn()
            .ok_or("cancel-cell root was not admitted")?;
        let mut events = turn.take_events()?;
        let control = turn.control();
        loop {
            let Ok(event) = events.recv().await else {
                let result = turn.result().await?;
                panic!(
                    "the turn ended before the process command started: {:?}\n{}",
                    result.outcome(),
                    describe_ledger(&result)
                );
            };
            if let AgentEventKind::ToolStarted { identity, .. } = event.kind()
                && identity.canonical_name().as_str() == PROCESS_SESSION_CAPABILITY_NAME
            {
                break;
            }
        }
        // Let the shell actually start sleeping before the cancel lands.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let requested = tokio::time::Instant::now();
        assert_eq!(control.cancel().await?, CancellationStatus::Requested);
        let result = turn.result().await?;
        let joined_after = requested.elapsed();
        assert_cancelled_and_quiescent(&result, &command_id)?;
        assert!(
            joined_after < Duration::from_secs(3),
            "a running nested command must join the cancel promptly, took {joined_after:?}"
        );
        let deliveries = lock_mutex(&evidence.deliveries).clone();
        assert!(
            deliveries.iter().any(|(id, ok, _)| id == "SLEEP" && !*ok),
            "the interrupted command is rejected to the cell, never resolved: {deliveries:?}"
        );
        agent.shutdown().await?;
        Ok(())
    })
}

/// A Code Cell a live owner fenced is closed by the next open, which is the
/// same recovery a crash gets: the evaluation is abandoned, never replayed, and
/// the turn keeps the one recovery-required result it already delivered.
#[tokio::test]
async fn a_reopen_abandons_a_fenced_code_cell_without_a_second_result() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 89_600)?;
    let agent = sessions
        .create(
            "create:code-cell-reopen-abandon",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("recovery-only code cell").await?;
    let turn_id = turn.id();
    let terminal = turn.result().await?;
    assert!(matches!(
        terminal.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let checkpoint = agent.checkpoint();
    let _ = agent.shutdown().await;
    let source = seed_code_cell_recovery_checkpoint(&store, checkpoint, turn_id)
        .await
        .map_err(|error| format!("seed failed: {error:?}"))?;
    let seeded = store.load_checkpoint(source.checkpoint_id).await?;
    assert!(matches!(
        super::super::turn_recovery::root_turn_closure_for_test(seeded.checkpoint())?,
        Some(super::super::turn_recovery::RootClosure::FinalizeRecorded {
            code_cell: Some(_),
            ..
        })
    ));
    let commits_before = lock_mutex(&store.state).stored_commits.len();
    let seeded_segments = seeded
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments()
        .len();
    let reopening_model = artifact_test_model(0, 89_650)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:code-cell-reopen-abandon",
            source,
            durable_artifact_test_builder(workspace, Arc::clone(&reopening_model)),
        )
        .await?
    else {
        return Err("a fenced Code Cell is abandoned by the reopen".into());
    };
    let report = reopened
        .abandoned_turn()
        .ok_or("the reopen reports the abandoned Code Cell")?;
    assert_eq!(
        lock_mutex(&store.state).stored_commits.len(),
        commits_before + 1,
        "Code Cell evidence and root terminal are published by one CAS"
    );
    assert_eq!(report.claim(), crate::AbandonedTurnClaim::RunningTool);
    assert_eq!(report.cause(), crate::AbandonmentCause::Interrupted);
    assert_eq!(report.operations().len(), 3);
    let head = reopened.checkpoint();
    assert!(matches!(
        head.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(
        head.manifest().canonical().history().segments().len(),
        seeded_segments + 1,
        "the abandoned evaluation's call and failure result are the one addition"
    );
    let root = head
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(turn_id))
        .ok_or("root command missing")?;
    let terminal = terminal_record_at_checkpoint(
        &head,
        &CommandId::new(root.command_id()).map_err(|_| "command identity")?,
    )?;
    assert!(
        matches!(
            terminal.outcome,
            DurableTerminalOutcome::RecoveryRequired(_)
        ),
        "the turn keeps the one result it delivered"
    );
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn historical_code_cell_continuation_resolves_only_its_finalized_evaluation() -> TestResult {
    for root_finalized in [false, true] {
        historical_code_cell_continuation_recovery(root_finalized, false).await?;
    }
    Ok(())
}

/// A two-round cell whose round-two continuation was already finalized by
/// root-boundary cleanup while a child of that continuation's attempt is
/// still dispatched: the child's attempt belongs to the cell, so explicit
/// abandonment closes the root instead of calling the ledger corrupt.
#[tokio::test]
async fn multi_round_code_cell_abandonment_admits_a_finalized_continuations_children() -> TestResult
{
    for root_finalized in [false, true] {
        historical_code_cell_continuation_recovery(root_finalized, true).await?;
    }
    Ok(())
}
