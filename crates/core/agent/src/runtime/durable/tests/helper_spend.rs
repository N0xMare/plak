//! Helper spend at the nested-call choke point (spec 01 §9.8.7): a fake
//! helper tool declares 4 physical requests per call and reports 1, so a
//! settle to the declaration instead of the report changes every count.

use std::collections::VecDeque;

use super::*;

const HELPER_TOOL: &str = "helper_probe";
const HELPER_CELL: &str = "helper-cell";
/// The fake helper's declared most physical requests per call.
const DECLARED: u32 = 4;
/// What a counted call of the fake helper reports.
const REPORTED: u32 = 1;

/// How one nested helper call behaves.
#[derive(Clone, Copy, Debug)]
enum HelperCall {
    /// Succeeds and reports [`REPORTED`] requests.
    Counted,
    /// Fails definitely without reporting a count.
    FailsUncounted,
}

impl HelperCall {
    const fn mode(self) -> &'static str {
        match self {
            Self::Counted => "counted",
            Self::FailsUncounted => "fails",
        }
    }
}

struct HelperProbe {
    dispatched: AtomicUsize,
}

impl plakous_tools::ToolExecutor for HelperProbe {
    fn execute(
        self: Arc<Self>,
        invocation: ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.dispatched.fetch_add(1, Ordering::AcqRel);
            let mode = invocation.arguments().value()["mode"]
                .as_str()
                .map(str::to_owned);
            match mode.as_deref() {
                Some("counted") => Ok(
                    ToolOutcome::success_text("helper answered")?.with_helper_requests(REPORTED)
                ),
                _ => ToolOutcome::failure_text("helper failed"),
            }
        })
    }
}

/// What each turn's cell saw: per nested call, `Ok` or the refusal text.
#[derive(Default)]
struct HelperEvidence {
    /// Calls per turn, in order; each session takes the next.
    script: Mutex<VecDeque<Vec<HelperCall>>>,
    /// Per turn, one entry per call in request order.
    observed: Mutex<Vec<Vec<Result<(), String>>>>,
}

struct HelperResponder {
    evidence: Arc<HelperEvidence>,
    delivered: Arc<AtomicUsize>,
    turn: usize,
}

impl HelperResponder {
    fn record(
        &self,
        outcome: Result<(), String>,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        if let Some(turn) = lock_mutex(&self.evidence.observed).get_mut(self.turn) {
            turn.push(outcome);
        }
        self.delivered.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted) })
    }
}

impl crate::local_composition::LocalCompositionNestedResponder for HelperResponder {
    fn resolve(
        self: Box<Self>,
        _value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        self.record(Ok(()))
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        self.record(Err(reason))
    }
}

/// One turn's cell: its calls, issued one at a time.
struct HelperCell {
    calls: Vec<HelperCall>,
    next: usize,
    turn: usize,
    delivered: Arc<AtomicUsize>,
    evidence: Arc<HelperEvidence>,
}

impl LocalCompositionEventReceiver for HelperCell {
    fn receive(&mut self) -> LocalCompositionSessionFuture<'_, Option<LocalCompositionEvent>> {
        Box::pin(async move {
            let Some(call) = self.calls.get(self.next).copied() else {
                return std::future::pending().await;
            };
            while self.delivered.load(Ordering::Acquire) < self.next {
                tokio::task::yield_now().await;
            }
            self.next += 1;
            Ok(Some(LocalCompositionEvent::NestedCall {
                call: crate::local_composition::LocalCompositionNestedCall {
                    cell_id: HELPER_CELL.to_owned(),
                    runtime_call_id: format!("helper-{}", self.next),
                    tool_name: HELPER_TOOL.to_owned(),
                    input: Some(json!({"mode": call.mode()})),
                },
                responder: Box::new(HelperResponder {
                    evidence: Arc::clone(&self.evidence),
                    delivered: Arc::clone(&self.delivered),
                    turn: self.turn,
                }),
            }))
        })
    }
}

struct HelperSession {
    calls: usize,
    delivered: Arc<AtomicUsize>,
}

impl LocalCompositionSession for HelperSession {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell> {
        let delivered = Arc::clone(&self.delivered);
        let calls = self.calls;
        Box::pin(async move {
            request.start_admission.admit().await?;
            Ok(LocalCompositionStartedCell {
                cell_id: HELPER_CELL.to_owned(),
                initial_response: Box::pin(async move {
                    while delivered.load(Ordering::Acquire) < calls {
                        tokio::task::yield_now().await;
                    }
                    Ok(LocalCompositionResponse::Result {
                        cell_id: HELPER_CELL.to_owned(),
                        items: vec![LocalCompositionContentItem::Text("helper cell".to_owned())],
                        error: None,
                    })
                }),
            })
        })
    }

    fn wait(
        &self,
        _request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome> {
        Box::pin(async { Err("helper fixture does not wait".to_owned()) })
    }

    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>> {
        Box::pin(async move {
            LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                LocalCompositionResponse::Terminated {
                    cell_id,
                    items: Vec::new(),
                },
            ))
        })
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }
}

struct HelperIntegration {
    descriptor: LocalCompositionDescriptor,
    evidence: Arc<HelperEvidence>,
}

impl LocalCompositionIntegration for HelperIntegration {
    fn runtime_contract(&self) -> crate::local_composition::LocalCompositionRuntimeContract {
        crate::local_composition::LocalCompositionRuntimeContract::PYTHON
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::RootTurnJoined
    }
    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::BytesPythonPtcV1
    }
    fn environment_identity(&self) -> Vec<u8> {
        b"plakous-agent/test-helper-spend-v1".to_vec()
    }
    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, String> {
        Ok(LocalCompositionExecuteInput {
            source: input
                .get("code")
                .and_then(serde_json::Value::as_str)
                .ok_or("helper source is absent")?
                .to_owned(),
            yield_time_ms: 10,
            max_output_tokens: 128,
        })
    }
    fn parse_wait(&self, _input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        Err("helper fixture does not wait".to_owned())
    }
    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        let calls = lock_mutex(&self.evidence.script)
            .pop_front()
            .unwrap_or_default();
        let turn = {
            let mut observed = lock_mutex(&self.evidence.observed);
            observed.push(Vec::new());
            observed.len() - 1
        };
        let delivered = Arc::new(AtomicUsize::new(0));
        let session = HelperSession {
            calls: calls.len(),
            delivered: Arc::clone(&delivered),
        };
        let events = HelperCell {
            calls,
            next: 0,
            turn,
            delivered,
            evidence: Arc::clone(&self.evidence),
        };
        Box::pin(async move {
            Ok(LocalCompositionSessionBundle {
                session: Box::new(session),
                events: Box::new(events),
            })
        })
    }
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }
    fn rebuild_descriptor(
        &self,
        _job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        helper_descriptor(registry, target, self.descriptor.helper_budget)
            .map_err(|error| error.to_string())
    }
    fn validate_configuration(
        &self,
        _job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        _registry: FrozenToolRegistry,
        _configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        if descriptor.identity != self.descriptor.identity
            || projection.digest() != descriptor.outer_projection.digest()
        {
            return Err("helper composition descriptor drifted".to_owned());
        }
        Ok(())
    }
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        _max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem> {
        items
    }
    fn max_yield_time_ms(&self) -> u64 {
        10_000
    }
    fn max_output_tokens(&self) -> usize {
        10_000
    }
}

fn helper_registry(probe: Arc<HelperProbe>, effect: ToolEffect) -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    registry.register(
        ToolRegistration::new(
            RegisteredToolId::new("test.helper.probe")?,
            ToolExecutorId::new("test.helper.probe.executor")?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.helper.probe", b"4"])?,
            ToolDefinition::new(
                ToolName::new(HELPER_TOOL)?,
                "1.0.0",
                "Offline helper-spend probe.",
                ToolInputContract::structured(json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"mode": {"type": "string"}},
                    "required": ["mode"],
                    "additionalProperties": false
                }))?,
                ToolOutputContract::model_content(128, 1_048_576)?,
                if effect == ToolEffect::ReadOnly {
                    ToolConcurrency::Parallel
                } else {
                    ToolConcurrency::Serial
                },
                effect,
            )?,
            probe,
        )
        .nested_eligible()
        .helper_requests(DECLARED),
    )?;
    PythonPtcV1::register_markers_into(&mut registry)?;
    Ok(registry.freeze())
}

/// The py-ptc descriptor over the helper probe, carrying `helper_budget`.
fn helper_descriptor(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    helper_budget: Option<u32>,
) -> TestResult<LocalCompositionDescriptor> {
    let exact = PythonPtcV1::compile(
        registry,
        target,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new(HELPER_TOOL)?],
    )?;
    Ok(LocalCompositionDescriptor {
        helper_budget,
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: vec![LocalCompositionToolDefinition {
            name: HELPER_TOOL.to_owned(),
            description: "Offline helper-spend probe.".to_owned(),
            input_schema: json!({"type": "object"}),
        }]
        .into(),
        identity: exact.identity(),
        execute_name: "exec_py".into(),
        wait_name: Some("wait_py".into()),
        marker_executor: ToolExecutorId::new("plakous.python-ptc.driver-marker.v1")?,
    })
}

/// Runs one agent turn per script entry on a py-ptc descriptor carrying
/// `helper_budget`, and returns what each turn's cell observed with the
/// count of dispatched helper calls.
async fn run_helper_turns(
    helper_budget: Option<u32>,
    effect: ToolEffect,
    turns: Vec<Vec<HelperCall>>,
) -> TestResult<(Vec<Vec<Result<(), String>>>, usize)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let probe = Arc::new(HelperProbe {
        dispatched: AtomicUsize::new(0),
    });
    let registry = helper_registry(Arc::clone(&probe), effect)?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let arguments = serde_json::to_string(&json!({"code": "helper spend"}))?;
    let scripted = (0..turns.len())
        .map(|turn| (turn * 2, "exec_py", arguments.clone()))
        .collect::<Vec<_>>();
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_7100, false)
            .with_scripted_tool_calls(scripted),
    );
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&["exec_py", "wait_py"]),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let target = composition_projection_target(&configured, &descriptor)?;
    let composition_descriptor = helper_descriptor(registry.clone(), target, helper_budget)?;
    let evidence = Arc::new(HelperEvidence {
        script: Mutex::new(turns.iter().cloned().collect()),
        ..HelperEvidence::default()
    });
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(HelperIntegration {
        descriptor: composition_descriptor.clone(),
        evidence: Arc::clone(&evidence),
    });
    let builder = code_cell_agent_builder(
        model.as_ref(),
        configured,
        registry,
        workspace,
        "test.helper-spend",
        composition_descriptor,
        integration,
    )?;
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions.create("create:helper-spend", builder).await?;
    for turn in 0..turns.len() {
        let result = agent
            .start(
                CommandId::new(format!("helper-spend:{turn}"))?,
                "spend helper requests",
            )
            .await?
            .into_turn()
            .ok_or("helper turn was not admitted")?
            .result()
            .await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "helper turn {turn} ended {:?}",
            result.outcome()
        );
    }
    agent.shutdown().await?;
    let observed = lock_mutex(&evidence.observed).clone();
    Ok((observed, probe.dispatched.load(Ordering::Acquire)))
}

fn refusal(used: u32) -> Result<(), String> {
    Err(format!(
        "helper spend limit reached ({used} of 32 requests this turn)"
    ))
}

/// Each call reserves 4 against 32 and settles to the 1 it reports, so
/// after k counted calls k are used and the next fits while k + 4 <= 32:
/// 29 fit, then every call is refused at 29 used. A settle to the
/// declaration would refuse the ninth call instead. A call without a count
/// keeps its whole reservation, and the next turn starts from zero.
#[tokio::test]
async fn helper_reservations_settle_to_reported_counts_and_reset_per_turn() -> TestResult {
    use HelperCall::{Counted, FailsUncounted};
    let first = vec![Counted; 32];
    // Three uncounted failures keep 4 each (12 used); counted calls then fit
    // while 12 + j + 4 <= 32: 17 fit, and the rest are refused at 29.
    let mut second = vec![FailsUncounted; 3];
    second.extend(vec![Counted; 19]);
    let (observed, dispatched) =
        run_helper_turns(Some(32), ToolEffect::ReadOnly, vec![first, second]).await?;
    assert_eq!(observed.len(), 2);
    let mut expected_first = vec![Ok(()); 29];
    expected_first.extend([refusal(29), refusal(29), refusal(29)]);
    assert_eq!(observed[0], expected_first);
    let mut expected_second = vec![Err("helper failed".to_owned()); 3];
    expected_second.extend(vec![Ok(()); 17]);
    expected_second.extend([refusal(29), refusal(29)]);
    assert_eq!(observed[1], expected_second);
    // A refused call never reached the executor.
    assert_eq!(dispatched, 29 + 3 + 17);
    Ok(())
}

/// A composition without a helper budget never refuses, whatever the
/// callee declares.
#[tokio::test]
async fn a_composition_without_a_helper_budget_never_refuses() -> TestResult {
    let (observed, dispatched) = run_helper_turns(
        None,
        ToolEffect::ReadOnly,
        vec![vec![HelperCall::FailsUncounted; 40]],
    )
    .await?;
    assert_eq!(observed, vec![vec![Err("helper failed".to_owned()); 40]]);
    assert_eq!(dispatched, 40);
    Ok(())
}

/// A mutating helper takes the serial nested path and settles the same way.
#[tokio::test]
async fn a_serial_helper_settles_at_the_same_choke_point() -> TestResult {
    let (observed, dispatched) = run_helper_turns(
        Some(32),
        ToolEffect::Mutating,
        vec![vec![HelperCall::Counted; 31]],
    )
    .await?;
    let mut expected = vec![Ok(()); 29];
    expected.extend([refusal(29), refusal(29)]);
    assert_eq!(observed, vec![expected]);
    assert_eq!(dispatched, 29);
    Ok(())
}
