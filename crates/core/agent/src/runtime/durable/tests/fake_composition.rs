//! Scripted composition ports shared by the durable law fixtures.
use super::*;

#[derive(Default)]
pub(super) struct CompositionCounters {
    pub(super) terminates: AtomicUsize,
    pub(super) session_shutdowns: AtomicUsize,
    pub(super) integration_shutdowns: AtomicUsize,
}

pub(super) enum FakeComposition {
    Neutral(FakeCompositionIntegration),
    Scheduler(DriverSchedulerIntegration),
    Cancel(CancelCellIntegration),
    Lineage(LineageIntegration),
}
impl LocalCompositionIntegration for FakeComposition {
    fn runtime_contract(&self) -> crate::local_composition::LocalCompositionRuntimeContract {
        match self {
            Self::Neutral(_) | Self::Scheduler(_) => {
                crate::local_composition::LocalCompositionRuntimeContract::JAVASCRIPT
            }
            Self::Cancel(fixture) => {
                if fixture.evidence.python_fixture.load(Ordering::Acquire) {
                    return crate::local_composition::LocalCompositionRuntimeContract::PYTHON;
                }
                crate::local_composition::LocalCompositionRuntimeContract::JAVASCRIPT
            }
            Self::Lineage(fixture) => {
                if matches!(fixture.class, LineagePtcClass::Python) {
                    crate::local_composition::LocalCompositionRuntimeContract::PYTHON
                } else {
                    crate::local_composition::LocalCompositionRuntimeContract::JAVASCRIPT
                }
            }
        }
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        match self {
            Self::Neutral(fixture) => fixture.lifecycle,
            Self::Scheduler(_) | Self::Lineage(_) => LocalCompositionLifecycle::RootTurnJoined,
            Self::Cancel(fixture) => {
                assert!(
                    !fixture
                        .evidence
                        .panic_cleanup_lifecycle
                        .load(Ordering::Acquire),
                    "injected local composition cleanup lifecycle panic"
                );
                if fixture
                    .evidence
                    .buffered_terminate_fixture
                    .load(Ordering::Acquire)
                {
                    return LocalCompositionLifecycle::AgentIncarnationPersistent;
                }
                LocalCompositionLifecycle::RootTurnJoined
            }
        }
    }
    fn result_projection(&self) -> LocalCompositionResultProjection {
        match self {
            Self::Neutral(_) | Self::Scheduler(_) => {
                LocalCompositionResultProjection::TokensScriptV1
            }
            Self::Cancel(fixture) => {
                if fixture.evidence.python_fixture.load(Ordering::Acquire) {
                    LocalCompositionResultProjection::BytesPythonPtcV1
                } else {
                    LocalCompositionResultProjection::TokensScriptV1
                }
            }
            Self::Lineage(fixture) => {
                if matches!(fixture.class, LineagePtcClass::Python) {
                    LocalCompositionResultProjection::BytesPythonPtcV1
                } else {
                    LocalCompositionResultProjection::TokensScriptV1
                }
            }
        }
    }
    fn environment_identity(&self) -> Vec<u8> {
        match self {
            Self::Neutral(_) => b"plakous-agent/test-neutral-composition-v1".to_vec(),
            Self::Scheduler(_) => b"plakous-agent/test-driver-scheduler-v1".to_vec(),
            Self::Cancel(_) => b"plakous-agent/test-cancel-cell-v1".to_vec(),
            Self::Lineage(fixture) => {
                format!("plakous-agent/test-lineage-{}-v1", fixture.class.label()).into_bytes()
            }
        }
    }
    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, String> {
        match self {
            Self::Neutral(_) => {
                let source = match input {
                    serde_json::Value::String(source) => source,
                    serde_json::Value::Object(mut input) => input
                        .remove("source")
                        .and_then(|source| source.as_str().map(str::to_owned))
                        .ok_or_else(|| "fake structured execute omitted source".to_owned())?,
                    _ => return Err("fake execute input has the wrong shape".to_owned()),
                };
                Ok(LocalCompositionExecuteInput {
                    source,
                    yield_time_ms: 10,
                    max_output_tokens: 128,
                })
            }
            Self::Scheduler(_) => Ok(LocalCompositionExecuteInput {
                source: input
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "scheduler source is absent".to_owned())?
                    .to_owned(),
                yield_time_ms: 10,
                max_output_tokens: 128,
            }),
            Self::Cancel(fixture) => {
                let source = match input {
                    serde_json::Value::String(source) => source,
                    other => other
                        .get(if fixture.evidence.python_fixture.load(Ordering::Acquire) {
                            "code"
                        } else {
                            "source"
                        })
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "cancel-cell source is absent".to_owned())?
                        .to_owned(),
                };
                Ok(LocalCompositionExecuteInput {
                    source,
                    yield_time_ms: 10,
                    max_output_tokens: 128,
                })
            }
            Self::Lineage(_) => {
                let source = match &input {
                    serde_json::Value::String(source) => source.clone(),
                    value => value
                        .get("source")
                        .or_else(|| value.get("code"))
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "lineage source is absent".to_owned())?
                        .to_owned(),
                };
                Ok(LocalCompositionExecuteInput {
                    source,
                    yield_time_ms: 10,
                    max_output_tokens: 128,
                })
            }
        }
    }
    fn parse_wait(&self, input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        match self {
            Self::Neutral(_) => {
                fn whole_u64(value: &serde_json::Value) -> Option<u64> {
                    if let Some(value) = value.as_u64() {
                        return Some(value);
                    }
                    let encoded = value.as_number()?.to_string();
                    encoded.strip_suffix(".0")?.parse().ok()
                }
                let cell_id = input
                    .get("cell_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "fake wait omitted cell_id".to_owned())?
                    .to_owned();
                let yield_time_ms = input
                    .get("yield_time_ms")
                    .and_then(whole_u64)
                    .ok_or_else(|| "fake wait yield_time_ms must be an integer".to_owned())?;
                let max_output_tokens =
                    input
                        .get("max_tokens")
                        .and_then(whole_u64)
                        .map_or(Ok(128_usize), |value| {
                            usize::try_from(value)
                                .map_err(|_| "fake max_tokens overflowed".to_owned())
                        })?;
                let parsed = LocalCompositionWaitInput {
                    cell_id,
                    yield_time_ms,
                    max_output_tokens,
                    terminate: input
                        .get("terminate")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                };
                Ok(parsed)
            }
            Self::Scheduler(_) => Err("scheduler fixture does not wait".to_owned()),
            Self::Cancel(_) => Ok(LocalCompositionWaitInput {
                cell_id: input
                    .get("cell_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "cancel-cell wait omitted cell_id".to_owned())?
                    .to_owned(),
                yield_time_ms: input
                    .get("yield_time_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(10),
                max_output_tokens: 128,
                terminate: input
                    .get("terminate")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            }),
            Self::Lineage(_) => Err("lineage fixture does not wait".to_owned()),
        }
    }
    fn create_session(
        &self,
        generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        match self {
            Self::Neutral(fixture) => {
                fixture
                    .evidence
                    .session_creates
                    .fetch_add(1, Ordering::AcqRel);
                lock_mutex(&fixture.evidence.generations).push(generation);
                lock_mutex(&fixture.evidence.order).push("create-session");
                let session = FakeCompositionSession {
                    evidence: Arc::clone(&fixture.evidence),
                    state: Mutex::new(None),
                    generation_lost: AtomicBool::new(false),
                };
                Box::pin(async move {
                    Ok(LocalCompositionSessionBundle {
                        session: Box::new(FakeSession::Neutral(session)),
                        events: Box::new(FakeEvents::Neutral(PendingCompositionEvents)),
                    })
                })
            }
            Self::Scheduler(fixture) => Box::pin(async move {
                Ok(LocalCompositionSessionBundle {
                    session: Box::new(FakeSession::Scheduler(DriverSchedulerSession {
                        evidence: Arc::clone(&fixture.evidence),
                    })),
                    events: Box::new(FakeEvents::Scheduler(DriverSchedulerEvents {
                        next: 0,
                        evidence: Arc::clone(&fixture.evidence),
                    })),
                })
            }),
            Self::Cancel(fixture) => Box::pin(async move {
                Ok(LocalCompositionSessionBundle {
                    session: Box::new(FakeSession::Cancel(CancelCellSession {
                        mode: fixture.mode,
                        evidence: Arc::clone(&fixture.evidence),
                    })),
                    events: Box::new(FakeEvents::Cancel(CancelCellEvents {
                        next: 0,
                        mode: fixture.mode,
                        evidence: Arc::clone(&fixture.evidence),
                    })),
                })
            }),
            Self::Lineage(fixture) => Box::pin(async move {
                Ok(LocalCompositionSessionBundle {
                    session: Box::new(FakeSession::Lineage(LineageSession {
                        evidence: Arc::clone(&fixture.evidence),
                        scenario: fixture.scenario,
                    })),
                    events: Box::new(FakeEvents::Lineage(LineageEvents {
                        next: 0,
                        evidence: Arc::clone(&fixture.evidence),
                        scenario: fixture.scenario,
                    })),
                })
            }),
        }
    }
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        match self {
            Self::Neutral(fixture) => {
                fixture
                    .evidence
                    .counters
                    .integration_shutdowns
                    .fetch_add(1, Ordering::AcqRel);
                lock_mutex(&fixture.evidence.order).push("integration-shutdown");
                Box::pin(async {
                    assert!(
                        !fixture
                            .evidence
                            .panic_integration_shutdown
                            .load(Ordering::Acquire),
                        "injected integration shutdown panic"
                    );
                    let child_exit = fixture
                        .evidence
                        .child_exit_on_integration_shutdown
                        .load(Ordering::Acquire)
                        .then(|| ChildProcessExit {
                            child: ChildProcessKind::JsEvalSidecar,
                            user_cpu: Duration::from_millis(3),
                            system_cpu: Duration::from_millis(1),
                            peak_rss_bytes: Some(4096),
                            escalation: ChildExitEscalation::Voluntary,
                        });
                    LocalCompositionShutdownResult::Joined { child_exit }
                })
            }
            Self::Scheduler(fixture) => {
                fixture
                    .evidence
                    .counters
                    .integration_shutdowns
                    .fetch_add(1, Ordering::AcqRel);
                Box::pin(async move {
                    if fixture.evidence.scenario
                        == DriverSchedulerScenario::DeliveryPanicsAfterIntegrationLoss
                    {
                        LocalCompositionShutdownResult::Lost(LocalCompositionSessionLoss::HostLost(
                            "injected earlier integration shutdown loss".to_owned(),
                        ))
                    } else {
                        LocalCompositionShutdownResult::Joined { child_exit: None }
                    }
                })
            }
            Self::Cancel(_) => {
                Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
            }
            Self::Lineage(fixture) => {
                fixture
                    .evidence
                    .counters
                    .integration_shutdowns
                    .fetch_add(1, Ordering::AcqRel);
                Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
            }
        }
    }
    fn rebuild_descriptor(
        &self,
        _job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        match self {
            Self::Neutral(fixture) => Ok(fixture.descriptor.clone()),
            Self::Scheduler(fixture) => {
                let nested_names = fixture
                    .descriptor
                    .nested_tools
                    .iter()
                    .map(|tool| ToolName::new(tool.name.clone()).map_err(|error| error.to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                let exact = ProductJavaScriptCompositionV1::compile(registry, target, nested_names)
                    .map_err(|error| error.to_string())?;
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
                    )
                    .map_err(|error| error.to_string())?,
                })
            }
            Self::Cancel(fixture) => {
                if fixture.mode.codex_process_catalog() {
                    return neutral_composition_descriptor(registry, target)
                        .map_err(|error| error.to_string());
                }
                if fixture.evidence.python_fixture.load(Ordering::Acquire) {
                    return cancel_cell_python_descriptor(registry, target)
                        .map_err(|error| error.to_string());
                }
                let nested_names = fixture
                    .descriptor
                    .nested_tools
                    .iter()
                    .map(|tool| ToolName::new(tool.name.clone()).map_err(|error| error.to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                let exact =
                    ProductJavaScriptCompositionV1::compile(registry.clone(), target, nested_names)
                        .map_err(|error| error.to_string())?;
                let mut descriptor =
                    cancel_cell_descriptor(&exact).map_err(|error| error.to_string())?;
                if fixture.evidence.mixed_tool_fixture.load(Ordering::Acquire) {
                    extend_cancel_cell_outer(&mut descriptor, registry)
                        .map_err(|error| error.to_string())?;
                }
                Ok(descriptor)
            }
            Self::Lineage(fixture) => {
                lineage_composition_descriptor(fixture.class, fixture.scenario, registry, target)
                    .map_err(|error| error.to_string())
            }
        }
    }
    fn validate_configuration(
        &self,
        _job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        _registry: FrozenToolRegistry,
        _configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        match self {
            Self::Neutral(fixture) => {
                if descriptor.identity != fixture.descriptor.identity
                    || projection.digest() != descriptor.outer_projection.digest()
                {
                    return Err("fake composition descriptor drifted".to_owned());
                }
                Ok(())
            }
            Self::Scheduler(fixture) => {
                if descriptor.identity != fixture.descriptor.identity
                    || projection.digest() != descriptor.outer_projection.digest()
                {
                    return Err("scheduler composition descriptor drifted".to_owned());
                }
                Ok(())
            }
            Self::Cancel(fixture) => {
                if descriptor.identity != fixture.descriptor.identity
                    || projection.digest() != descriptor.outer_projection.digest()
                {
                    return Err("cancel-cell composition descriptor drifted".to_owned());
                }
                Ok(())
            }
            Self::Lineage(fixture) => {
                if descriptor.identity != fixture.descriptor.identity
                    || projection.digest() != descriptor.outer_projection.digest()
                {
                    return Err("lineage composition descriptor drifted".to_owned());
                }
                Ok(())
            }
        }
    }
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        _max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem> {
        match self {
            Self::Neutral(_) | Self::Scheduler(_) | Self::Cancel(_) | Self::Lineage(_) => items,
        }
    }
    fn max_yield_time_ms(&self) -> u64 {
        match self {
            Self::Neutral(_) | Self::Scheduler(_) | Self::Cancel(_) | Self::Lineage(_) => 10_000,
        }
    }
    fn max_output_tokens(&self) -> usize {
        match self {
            Self::Neutral(_) | Self::Scheduler(_) | Self::Cancel(_) | Self::Lineage(_) => 10_000,
        }
    }
    fn max_outstanding_nested_calls(&self) -> usize {
        match self {
            Self::Neutral(_) | Self::Cancel(_) => 16,
            Self::Scheduler(fixture) => {
                if fixture.evidence.scenario == DriverSchedulerScenario::EdgePublicationFailure {
                    3
                } else {
                    2
                }
            }
            Self::Lineage(_) => 2,
        }
    }
}

enum FakeSession {
    Neutral(FakeCompositionSession),
    Scheduler(DriverSchedulerSession),
    Cancel(CancelCellSession),
    Lineage(LineageSession),
}
impl LocalCompositionSession for FakeSession {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell> {
        match self {
            Self::Neutral(fixture) => {
                fixture.evidence.executes.fetch_add(1, Ordering::AcqRel);
                Box::pin(async move {
                    if fixture.evidence.delay_source.swap(false, Ordering::AcqRel) {
                        fixture.evidence.source_waiting.notify_one();
                        fixture.evidence.source_release.notified().await;
                    }
                    request.start_admission.admit().await?;
                    // This fake boundary stands in for the host's SourceAccepted edge:
                    // after this increment the evaluator may have begun mutating its heap.
                    fixture
                        .evidence
                        .physical_cell_starts
                        .fetch_add(1, Ordering::AcqRel);
                    lock_mutex(&fixture.evidence.order).push("execute");
                    let mut started_cell_id = "C";
                    let response = match request.source.as_str() {
                        "yield once" => LocalCompositionResponse::Yielded {
                            cell_id: "C".to_owned(),
                            items: vec![LocalCompositionContentItem::Text("yielded".to_owned())],
                        },
                        // A cell that never finishes: every resume yields again, so the
                        // root turn can only end at its own deadline.
                        "yield forever" => {
                            started_cell_id = FOREVER_YIELD_CELL;
                            LocalCompositionResponse::Yielded {
                                cell_id: FOREVER_YIELD_CELL.to_owned(),
                                items: vec![LocalCompositionContentItem::Text(
                                    "yielded".to_owned(),
                                )],
                            }
                        }
                        // A cell that emits an empty text item among ordinary ones,
                        // exactly as user code calling `text("")` does.
                        // A cell that raises and takes its evaluator with it: the
                        // outcome is definite, but the generation cannot serve again.
                        "raise and lose the generation" => {
                            fixture.generation_lost.store(true, Ordering::Release);
                            LocalCompositionResponse::Result {
                                cell_id: "C".to_owned(),
                                items: vec![LocalCompositionContentItem::Text(
                                    "printed".to_owned(),
                                )],
                                error: Some("cell raised".to_owned()),
                            }
                        }
                        "emit empty text" => LocalCompositionResponse::Result {
                            cell_id: "C".to_owned(),
                            items: vec![
                                LocalCompositionContentItem::Text(String::new()),
                                LocalCompositionContentItem::Text("kept output".to_owned()),
                                LocalCompositionContentItem::Text(String::new()),
                            ],
                            error: None,
                        },
                        "store bounded state" => {
                            *lock_mutex(&fixture.state) = Some("survived".to_owned());
                            LocalCompositionResponse::Result {
                                cell_id: "C".to_owned(),
                                items: vec![LocalCompositionContentItem::Text("stored".to_owned())],
                                error: None,
                            }
                        }
                        "load bounded state" => {
                            let value = lock_mutex(&fixture.state)
                                .clone()
                                .unwrap_or_else(|| "missing".to_owned());
                            lock_mutex(&fixture.evidence.loaded_values).push(value.clone());
                            LocalCompositionResponse::Result {
                                cell_id: "C".to_owned(),
                                items: vec![LocalCompositionContentItem::Text(value)],
                                error: None,
                            }
                        }
                        _ => {
                            return Err(LocalCompositionSessionLoss::ProtocolViolation(
                                "unexpected fake source".to_owned(),
                            ));
                        }
                    };
                    Ok(LocalCompositionStartedCell {
                        cell_id: started_cell_id.to_owned(),
                        initial_response: Box::pin(async move { Ok(response) }),
                    })
                })
            }
            Self::Scheduler(fixture) => Box::pin(async move {
                assert!(
                    fixture.evidence.scenario
                        != DriverSchedulerScenario::PanicBeforeSourceAdmission,
                    "injected panic with extracted resource pair before source admission"
                );
                request.start_admission.admit().await?;
                fixture
                    .evidence
                    .source_starts
                    .fetch_add(1, Ordering::AcqRel);
                assert!(
                    fixture.evidence.scenario != DriverSchedulerScenario::PanicAfterSourceAdmission,
                    "injected panic after source admission before cell acknowledgement"
                );
                Ok(LocalCompositionStartedCell {
                    cell_id: "scheduler-cell".to_owned(),
                    initial_response: Box::pin(std::future::pending()),
                })
            }),
            Self::Cancel(fixture) => {
                let mode = fixture.mode;
                Box::pin(async move {
                    request.start_admission.admit().await?;
                    fixture.evidence.executes.fetch_add(1, Ordering::AcqRel);
                    if mode == CancelCellMode::ExecutePending {
                        fixture
                            .evidence
                            .execute_started
                            .store(true, Ordering::Release);
                        return std::future::pending().await;
                    }
                    let initial_response: LocalCompositionSessionFuture<
                        'static,
                        LocalCompositionResponse,
                    > = match mode {
                        CancelCellMode::ExecutePending => unreachable!("startup already parked"),
                        CancelCellMode::InitialResponsePending => {
                            let evidence = Arc::clone(&fixture.evidence);
                            Box::pin(async move {
                                evidence
                                    .initial_response_started
                                    .store(true, Ordering::Release);
                                wait_for_scheduler_fact(&evidence.release_cell).await;
                                Ok(LocalCompositionResponse::Result {
                                    cell_id: "cancel-cell".to_owned(),
                                    items: vec![LocalCompositionContentItem::Text(
                                        "cell settled".to_owned(),
                                    )],
                                    error: None,
                                })
                            })
                        }
                        CancelCellMode::ExecInFlight
                            if fixture
                                .evidence
                                .nested_deadline_fixture
                                .load(Ordering::Acquire) =>
                        {
                            let evidence = Arc::clone(&fixture.evidence);
                            Box::pin(async move {
                                wait_for_scheduler_fact(&evidence.release_cell).await;
                                Ok(LocalCompositionResponse::Result {
                                    cell_id: "cancel-cell".to_owned(),
                                    items: Vec::new(),
                                    error: None,
                                })
                            })
                        }
                        CancelCellMode::ProcessYield
                            if fixture
                                .evidence
                                .continue_after_terminal
                                .load(Ordering::Acquire) =>
                        {
                            let evidence = Arc::clone(&fixture.evidence);
                            Box::pin(async move {
                                loop {
                                    if lock_mutex(&evidence.deliveries)
                                        .iter()
                                        .any(|(id, ok, _)| id == "TERMINAL" && *ok)
                                    {
                                        break;
                                    }
                                    tokio::task::yield_now().await;
                                }
                                // Stay computing beyond nested settlement: a spurious Notice
                                // would yield here before the cell's next user-visible result.
                                wait_for_scheduler_fact(&evidence.release_cell).await;
                                Ok(LocalCompositionResponse::Result {
                                    cell_id: "cancel-cell".to_owned(),
                                    items: vec![LocalCompositionContentItem::Text(
                                        "continued after terminal observation".to_owned(),
                                    )],
                                    error: None,
                                })
                            })
                        }
                        CancelCellMode::ExecInFlight
                        | CancelCellMode::ProcessInFlight
                        | CancelCellMode::ProcessYield => Box::pin(std::future::pending()),
                        CancelCellMode::WaitWindow => Box::pin(async move {
                            Ok(LocalCompositionResponse::Yielded {
                                cell_id: "cancel-cell".to_owned(),
                                items: vec![LocalCompositionContentItem::Text(
                                    "yielded".to_owned(),
                                )],
                            })
                        }),
                    };
                    Ok(LocalCompositionStartedCell {
                        cell_id: "cancel-cell".to_owned(),
                        initial_response,
                    })
                })
            }
            Self::Lineage(fixture) => {
                let evidence = Arc::clone(&fixture.evidence);
                let nested_calls = fixture.scenario.nested_calls();
                Box::pin(async move {
                    request.start_admission.admit().await?;
                    evidence.cell_running.store(true, Ordering::Release);
                    Ok(LocalCompositionStartedCell {
                        cell_id: LINEAGE_CELL_ID.to_owned(),
                        initial_response: Box::pin(async move {
                            while evidence.delivered.load(Ordering::Acquire) < nested_calls {
                                tokio::task::yield_now().await;
                            }
                            Ok(LocalCompositionResponse::Result {
                                cell_id: LINEAGE_CELL_ID.to_owned(),
                                items: vec![LocalCompositionContentItem::Text(
                                    "lineage cell finished".to_owned(),
                                )],
                                error: None,
                            })
                        }),
                    })
                })
            }
        }
    }
    fn wait(
        &self,
        request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome> {
        match self {
            Self::Neutral(fixture) => {
                fixture.evidence.waits.fetch_add(1, Ordering::AcqRel);
                lock_mutex(&fixture.evidence.order).push("wait");
                Box::pin(async move {
                    if request.cell_id == FOREVER_YIELD_CELL {
                        // Stay within the requested observation window while proving
                        // elapsed root-turn time does not cap successive resumes.
                        tokio::time::advance(FOREVER_YIELD_STEP).await;
                        return Ok(LocalCompositionWaitOutcome::Live(
                            LocalCompositionResponse::Yielded {
                                cell_id: FOREVER_YIELD_CELL.to_owned(),
                                items: vec![LocalCompositionContentItem::Text(
                                    "still yielded".to_owned(),
                                )],
                            },
                        ));
                    }
                    if request.cell_id != "C" {
                        return Err("unexpected fake cell".to_owned());
                    }
                    if fixture.evidence.fail_wait.load(Ordering::Acquire) {
                        return Err("injected wait transport failure".to_owned());
                    }
                    Ok(LocalCompositionWaitOutcome::Live(
                        LocalCompositionResponse::Result {
                            cell_id: "C".to_owned(),
                            items: vec![LocalCompositionContentItem::Text("joined".to_owned())],
                            error: None,
                        },
                    ))
                })
            }
            Self::Scheduler(_) => {
                Box::pin(async { Err("scheduler fixture does not wait".to_owned()) })
            }
            Self::Cancel(fixture) => {
                fixture.evidence.wait_started.store(true, Ordering::Release);
                fixture.evidence.wait_count.fetch_add(1, Ordering::AcqRel);
                if fixture.evidence.steer_fixture.load(Ordering::Acquire)
                    || fixture.evidence.long_wait_fixture.load(Ordering::Acquire)
                    || fixture
                        .evidence
                        .nested_deadline_fixture
                        .load(Ordering::Acquire)
                {
                    return Box::pin(async move {
                        wait_for_scheduler_fact(&fixture.evidence.release_cell).await;
                        Ok(LocalCompositionWaitOutcome::Live(
                            LocalCompositionResponse::Result {
                                cell_id: "cancel-cell".to_owned(),
                                items: vec![LocalCompositionContentItem::Text(
                                    "cell settled".to_owned(),
                                )],
                                error: None,
                            },
                        ))
                    });
                }
                Box::pin(std::future::pending())
            }
            Self::Lineage(_) => Box::pin(async { Err("lineage fixture does not wait".to_owned()) }),
        }
    }
    fn generation_lost(&self) -> bool {
        match self {
            Self::Neutral(fixture) => fixture.generation_lost.load(Ordering::Acquire),
            Self::Scheduler(_) | Self::Cancel(_) | Self::Lineage(_) => false,
        }
    }
    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>> {
        match self {
            Self::Neutral(fixture) => {
                fixture
                    .evidence
                    .counters
                    .terminates
                    .fetch_add(1, Ordering::AcqRel);
                lock_mutex(&fixture.evidence.order).push("terminate");
                let lost = fixture.evidence.lose_terminate.load(Ordering::Acquire);
                Box::pin(async move {
                    if lost {
                        return LocalCompositionTerminateResult::Lost(
                            LocalCompositionSessionLoss::ProtocolViolation(
                                "injected terminate loss".to_owned(),
                            ),
                        );
                    }
                    LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                        LocalCompositionResponse::Terminated {
                            cell_id: "C".to_owned(),
                            items: Vec::new(),
                        },
                    ))
                })
            }
            Self::Scheduler(fixture) => {
                fixture
                    .evidence
                    .counters
                    .terminates
                    .fetch_add(1, Ordering::AcqRel);
                Box::pin(async move {
                    LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                        LocalCompositionResponse::Terminated {
                            cell_id,
                            items: Vec::new(),
                        },
                    ))
                })
            }
            Self::Lineage(fixture) => {
                fixture
                    .evidence
                    .counters
                    .terminates
                    .fetch_add(1, Ordering::AcqRel);
                Box::pin(async move {
                    LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                        LocalCompositionResponse::Terminated {
                            cell_id,
                            items: Vec::new(),
                        },
                    ))
                })
            }
            Self::Cancel(fixture) => {
                fixture
                    .evidence
                    .counters
                    .terminates
                    .fetch_add(1, Ordering::AcqRel);
                assert!(
                    !fixture.evidence.panic_terminate.load(Ordering::Acquire),
                    "injected local composition terminate panic"
                );
                Box::pin(async move {
                    if fixture
                        .evidence
                        .buffered_terminate_fixture
                        .load(Ordering::Acquire)
                    {
                        return LocalCompositionTerminateResult::Joined(
                            LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Result {
                                cell_id,
                                items: vec![LocalCompositionContentItem::Text(
                                    "buffered result".into(),
                                )],
                                error: None,
                            }),
                        );
                    }
                    LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                        LocalCompositionResponse::Terminated {
                            cell_id,
                            items: Vec::new(),
                        },
                    ))
                })
            }
        }
    }
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        match self {
            Self::Neutral(fixture) => {
                fixture
                    .evidence
                    .counters
                    .session_shutdowns
                    .fetch_add(1, Ordering::AcqRel);
                lock_mutex(&fixture.evidence.order).push("session-shutdown");
                assert!(
                    !fixture
                        .evidence
                        .panic_session_shutdown
                        .load(Ordering::Acquire),
                    "injected session shutdown panic"
                );
                let lost = fixture
                    .evidence
                    .lose_session_shutdown
                    .load(Ordering::Acquire);
                Box::pin(async move {
                    if lost {
                        LocalCompositionShutdownResult::Lost(LocalCompositionSessionLoss::HostLost(
                            "injected session shutdown loss".to_owned(),
                        ))
                    } else {
                        LocalCompositionShutdownResult::Joined { child_exit: None }
                    }
                })
            }
            Self::Scheduler(fixture) => {
                fixture
                    .evidence
                    .counters
                    .session_shutdowns
                    .fetch_add(1, Ordering::AcqRel);
                Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
            }
            Self::Lineage(fixture) => {
                fixture
                    .evidence
                    .counters
                    .session_shutdowns
                    .fetch_add(1, Ordering::AcqRel);
                Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
            }
            Self::Cancel(fixture) => {
                fixture
                    .evidence
                    .counters
                    .session_shutdowns
                    .fetch_add(1, Ordering::AcqRel);
                let loss = lock_mutex(&fixture.evidence.shutdown_loss).clone();
                Box::pin(async move {
                    match loss {
                        Some(loss) => LocalCompositionShutdownResult::Lost(
                            LocalCompositionSessionLoss::HostLost(loss),
                        ),
                        None => LocalCompositionShutdownResult::Joined { child_exit: None },
                    }
                })
            }
        }
    }
}

enum FakeEvents {
    Neutral(PendingCompositionEvents),
    Scheduler(DriverSchedulerEvents),
    Cancel(CancelCellEvents),
    Lineage(LineageEvents),
}
impl LocalCompositionEventReceiver for FakeEvents {
    fn receive(&mut self) -> LocalCompositionSessionFuture<'_, Option<LocalCompositionEvent>> {
        match self {
            Self::Neutral(_) => Box::pin(std::future::pending()),
            Self::Scheduler(fixture) => {
                Box::pin(async move {
                    assert!(
                        fixture.evidence.scenario
                            != DriverSchedulerScenario::PanicAfterCellAcknowledgement,
                        "injected panic with acknowledged cell and extracted resource pair"
                    );
                    if matches!(
                        fixture.evidence.scenario,
                        DriverSchedulerScenario::ReadOnlyLeaseExpiry
                            | DriverSchedulerScenario::MutatingLeaseExpiry
                    ) {
                        if fixture.next >= 2 {
                            return std::future::pending().await;
                        }
                        let identity = if fixture.next == 0 {
                            "EXPIRY"
                        } else {
                            "EXPIRY_DUPLICATE"
                        };
                        let tool_name = if fixture.evidence.scenario
                            == DriverSchedulerScenario::ReadOnlyLeaseExpiry
                        {
                            "read_a"
                        } else {
                            "mutate_one"
                        };
                        fixture.next += 1;
                        return Ok(Some(LocalCompositionEvent::NestedCall {
                            call: crate::local_composition::LocalCompositionNestedCall {
                                cell_id: "scheduler-cell".to_owned(),
                                runtime_call_id: "EXPIRY".to_owned(),
                                tool_name: tool_name.to_owned(),
                                input: Some(json!({})),
                            },
                            responder: Box::new(DriverSchedulerResponder {
                                identity: identity.to_owned(),
                                evidence: Arc::clone(&fixture.evidence),
                            }),
                        }));
                    }
                    if fixture.evidence.scenario == DriverSchedulerScenario::EdgePublicationFailure
                        || fixture.evidence.scenario == DriverSchedulerScenario::DeliveryHostLost
                        || fixture.evidence.scenario.panics_during_delivery()
                    {
                        let event = match fixture.next {
                            0 => fixture.nested("A", "read_a"),
                            1 => {
                                wait_for_scheduler_fact(&fixture.evidence.read_a_started).await;
                                LocalCompositionEvent::NestedCall {
                                    call: crate::local_composition::LocalCompositionNestedCall {
                                        cell_id: "scheduler-cell".to_owned(),
                                        runtime_call_id: "A".to_owned(),
                                        tool_name: "read_a".to_owned(),
                                        input: Some(json!({})),
                                    },
                                    responder: Box::new(DriverSchedulerResponder {
                                        identity: "A_DUPLICATE".to_owned(),
                                        evidence: Arc::clone(&fixture.evidence),
                                    }),
                                }
                            }
                            2 if fixture.evidence.scenario
                                == DriverSchedulerScenario::EdgePublicationFailure =>
                            {
                                fixture.nested("M1", "mutate_one")
                            }
                            _ => {
                                // The next broker poll proves all preceding requests
                                // were admitted before releasing the first result.
                                fixture
                                    .evidence
                                    .release_read_a
                                    .store(true, Ordering::Release);
                                return std::future::pending().await;
                            }
                        };
                        fixture.next += 1;
                        return Ok(Some(event));
                    }
                    let event = match fixture.next {
                        0 => fixture.nested("A", "read_a"),
                        1 => {
                            wait_for_scheduler_fact(&fixture.evidence.read_a_started).await;
                            fixture.nested("B", "read_b")
                        }
                        2 => {
                            wait_for_scheduler_fact(&fixture.evidence.read_b_started).await;
                            fixture
                                .evidence
                                .cap_requested
                                .store(true, Ordering::Release);
                            fixture.nested("CAP", "read_a")
                        }
                        3 => {
                            wait_for_scheduler_fact(&fixture.evidence.read_b_delivered).await;
                            fixture.nested("M1", "mutate_one")
                        }
                        4 => {
                            wait_for_scheduler_count(&fixture.evidence.mutations_done, 1).await;
                            fixture.nested("M2", "mutate_two")
                        }
                        5 => {
                            wait_for_scheduler_count(&fixture.evidence.mutations_done, 2).await;
                            fixture.nested_with_input(
                                "INVALID",
                                "read_a",
                                json!({"unexpected": true}),
                            )
                        }
                        6 => {
                            wait_for_scheduler_fact(&fixture.evidence.invalid_rejected).await;
                            fixture.nested_with_input(
                                "INVALID_MUTATION",
                                "mutate_one",
                                json!({"unexpected": true}),
                            )
                        }
                        7 => {
                            wait_for_scheduler_fact(&fixture.evidence.serial_invalid_delivered)
                                .await;
                            fixture.nested("CANCEL", "read_cancel")
                        }
                        _ => return std::future::pending().await,
                    };
                    fixture.next += 1;
                    Ok(Some(event))
                })
            }
            Self::Cancel(fixture) => Box::pin(async move {
                if matches!(
                    fixture.mode,
                    CancelCellMode::ExecutePending | CancelCellMode::InitialResponsePending
                ) {
                    return std::future::pending().await;
                }
                if fixture.mode == CancelCellMode::WaitWindow {
                    if !fixture
                        .evidence
                        .nested_deadline_fixture
                        .load(Ordering::Acquire)
                    {
                        return std::future::pending().await;
                    }
                    wait_for_scheduler_fact(&fixture.evidence.wait_started).await;
                }
                if let Some(input) = fixture.mode.process_input() {
                    if (fixture.next == 1
                        || (fixture.next == 2
                            && fixture.evidence.repeat_poll.load(Ordering::Acquire)))
                        && fixture.evidence.terminal_poll.load(Ordering::Acquire)
                    {
                        let handle = loop {
                            let delivered = lock_mutex(&fixture.evidence.deliveries)
                                .iter()
                                .find(|(identity, accepted, _)| identity == "SLEEP" && *accepted)
                                .map(|(_, _, value)| value.clone());
                            if let Some(delivered) = delivered {
                                let result: serde_json::Value = serde_json::from_str(&delivered)
                                    .map_err(|error| {
                                        LocalCompositionSessionLoss::ProtocolViolation(
                                            error.to_string(),
                                        )
                                    })?;
                                break result["session_id"].clone();
                            }
                            tokio::task::yield_now().await;
                        };
                        let identity = if fixture.next == 2 {
                            "REPEAT"
                        } else {
                            "TERMINAL"
                        };
                        if fixture.next == 2 {
                            loop {
                                if lock_mutex(&fixture.evidence.deliveries)
                                    .iter()
                                    .any(|(id, ok, _)| id == "TERMINAL" && *ok)
                                {
                                    break;
                                }
                                tokio::task::yield_now().await;
                            }
                        }
                        fixture.next += 1;
                        return Ok(Some(LocalCompositionEvent::NestedCall {
                            call: crate::local_composition::LocalCompositionNestedCall {
                                cell_id: "cancel-cell".to_owned(),
                                runtime_call_id: identity.to_owned(),
                                tool_name: "write_stdin".to_owned(),
                                input: Some(json!({"session_id": handle,
                                                    "chars": if fixture.evidence.empty_poll.load(Ordering::Acquire) { "" } else { "finish\n" },
                                                    "yield_time_ms": if fixture.evidence.empty_poll.load(Ordering::Acquire) { 5000 } else { 1000 }})),
                            },
                            responder: Box::new(CancelCellResponder {
                                identity: identity.to_owned(),
                                evidence: Arc::clone(&fixture.evidence),
                            }),
                        }));
                    }
                    if fixture.next != 0 {
                        return std::future::pending().await;
                    }
                    let input = if fixture.evidence.steer_fixture.load(Ordering::Acquire) {
                        json!({"cmd": "while [ ! -f ready ]; do sleep 0.02; done; exit 7",
                                        "yield_time_ms": if fixture.evidence.empty_poll.load(Ordering::Acquire) { 300 } else { 10000 }})
                    } else if fixture.evidence.terminal_poll.load(Ordering::Acquire) {
                        json!({"cmd": "stty -echo; read line; printf terminal; exit 7", "tty": true, "yield_time_ms": 300})
                    } else {
                        input
                    };
                    fixture.next += 1;
                    return Ok(Some(LocalCompositionEvent::NestedCall {
                        call: crate::local_composition::LocalCompositionNestedCall {
                            cell_id: "cancel-cell".to_owned(),
                            runtime_call_id: "SLEEP".to_owned(),
                            tool_name: "exec_command".to_owned(),
                            input: Some(input),
                        },
                        responder: Box::new(CancelCellResponder {
                            identity: "SLEEP".to_owned(),
                            evidence: Arc::clone(&fixture.evidence),
                        }),
                    }));
                }
                let event = match fixture.next {
                    0 => fixture.nested("DONE", "nested_done"),
                    1 => {
                        wait_for_scheduler_fact(&fixture.evidence.done_delivered).await;
                        fixture.nested("BLOCK", "nested_block")
                    }
                    _ => return std::future::pending().await,
                };
                fixture.next += 1;
                Ok(Some(event))
            }),
            Self::Lineage(fixture) => {
                Box::pin(async move {
                    if fixture.next >= fixture.scenario.nested_calls() {
                        return std::future::pending().await;
                    }
                    // Serialize the cell's requests so acceptance order, and hence
                    // the minted `request_sequence`, is deterministic.
                    while fixture.evidence.delivered.load(Ordering::Acquire) < fixture.next {
                        tokio::task::yield_now().await;
                    }
                    let event = LocalCompositionEvent::NestedCall {
                        call: crate::local_composition::LocalCompositionNestedCall {
                            cell_id: LINEAGE_CELL_ID.to_owned(),
                            runtime_call_id: format!("lineage-{}", fixture.next),
                            tool_name: fixture.scenario.nested_name().to_owned(),
                            input: Some(if fixture.evidence.invalid_read {
                                json!({"path":false})
                            } else {
                                fixture.scenario.nested_input(fixture.next)
                            }),
                        },
                        responder: Box::new(LineageResponder {
                            evidence: Arc::clone(&fixture.evidence),
                        }),
                    };
                    fixture.next += 1;
                    Ok(Some(event))
                })
            }
        }
    }
}
