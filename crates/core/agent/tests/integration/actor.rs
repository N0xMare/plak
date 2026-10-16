//! Deterministic actor, control, tool, and terminal-result conformance.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::time::Duration;

use plakous_agent::internal::{
    Agent, AgentBuilder, AgentConfig, AgentErrorKind, AgentEventKind, AgentRequestPolicy,
    CancellationStatus, CommandAckDisposition, CommandId, ControlErrorKind, EffectDecisionRef,
    EffectDecisionResolution, EffectDecisionStatus, EventVisibility, SteeringStatus,
    SubscriptionEnd, SubscriptionLimits, ToolCallerClass, ToolCallerLineageView, Turn, TurnCommand,
    TurnEvents, TurnResult, TurnResultOutcome, ViewRequest,
};
use plakous_model::{
    Actor, AttemptId, BlockCompleted, BlockStart, BlockStarted, CachePolicy, CallId,
    CanonicalHistory, Capability, CapabilityRequirement, CompiledProviderConfiguration,
    ConfiguredLanguageModel, ConfiguredTool, ContentBlock, ConversationItem, ConversationView,
    EntryId, FinishDetails, FinishReason, FixedClock, GenerationLimits, HistorySegment,
    HistorySegmentKind, ModelEvent, ModelRequest, ModelResponse, OutputBoundDisposition,
    OutputLocation, Provenance, ReasoningBlock, ReasoningDelta, ReasoningPolicy,
    ReasoningVisibility, RequestMetadata, ResolvedModelDescriptor, ResponseStarted, RunId,
    RuntimeModelAuthority, SequenceIdGenerator, TextBlock, TextDelta, TextPhase, ToolCallBlock,
    ToolCallState, ToolChoice, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolEffect,
    ToolExecutionDisposition, ToolExecutionOwner, ToolInputContract, ToolInputDelta, ToolName,
    ToolOutputContract, ToolResultBlock, ToolResultKind, ToolResultPart, TurnId, ValidationMode,
    WorkingProjection,
};
use plakous_tools::{
    EffectDecisionPrompt, FrozenToolRegistry, RegisteredToolId, ToolContext, ToolExecutor,
    ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome, ToolPolicy, ToolPolicyDecision,
    ToolPolicyFuture, ToolPolicyRequest, ToolPolicyRevision, ToolRegistration, ToolRegistryBuilder,
    ToolRuntimeError, ToolRuntimeErrorKind, WorkspacePolicy,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::support::{
    self, DescriptorIds, Fixture, MAX_OUTPUT_TOKENS, TestResult, assistant_text_item,
    generation_limits, id, timestamp, user_text_item, workspace,
};

const PROMPT: &str = "hello";

const FIXTURE: Fixture = Fixture {
    label: "actor",
    provider_semantics: b"actor-provider-semantics-v1",
};
const DESCRIPTOR: DescriptorIds = DescriptorIds {
    protocol: "test.protocol",
    route: "test.route",
    model: "test.model",
    family: "test.actor",
    profile: "test.profile",
};

fn runtime_authority(descriptor: &ResolvedModelDescriptor) -> TestResult<RuntimeModelAuthority> {
    support::runtime_authority(&FIXTURE, descriptor)
}

fn provider_configuration_with_output_bound(
    descriptor: &ResolvedModelDescriptor,
    tools: &[ToolDefinition],
    max_output_tokens: u32,
) -> TestResult<CompiledProviderConfiguration> {
    support::provider_configuration(
        &FIXTURE,
        descriptor,
        &runtime_authority(descriptor)?,
        tools,
        CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        },
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(max_output_tokens).ok_or("nonzero actor output bound")?,
        ),
    )
}

fn descriptor(with_tools: bool, with_reasoning: bool) -> TestResult<ResolvedModelDescriptor> {
    let mut capabilities = vec![Capability::Streaming, Capability::Caching];
    if with_tools {
        capabilities.push(Capability::Tools);
    }
    if with_reasoning {
        capabilities.push(Capability::Reasoning);
    }
    support::descriptor(&FIXTURE, &DESCRIPTOR, &capabilities)
}

fn scripted_model(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = plakous_test_support::ModelScenario>,
) -> TestResult<plakous_test_support::ScriptedModel> {
    support::scripted_model(&FIXTURE, descriptor, scenarios)
}

fn identity_sources() -> TestResult<(Arc<FixedClock>, Arc<SequenceIdGenerator>)> {
    support::identity_sources(256)
}

fn agent_builder(
    model: Arc<plakous_test_support::ScriptedModel>,
    tools: FrozenToolRegistry,
    workspace: Arc<WorkspacePolicy>,
) -> TestResult<AgentBuilder> {
    let configured = configured_model(model, &tools)?;
    Ok(
        AgentBuilder::new(configured, tools, workspace).request_policy(AgentRequestPolicy::new(
            generation_limits()?,
            ReasoningPolicy::default(),
        )),
    )
}

fn provider_configuration(
    descriptor: &ResolvedModelDescriptor,
    tools: &[ToolDefinition],
) -> TestResult<CompiledProviderConfiguration> {
    provider_configuration_with_output_bound(descriptor, tools, MAX_OUTPUT_TOKENS)
}

fn configured_model(
    model: Arc<plakous_test_support::ScriptedModel>,
    tools: &FrozenToolRegistry,
) -> TestResult<ConfiguredLanguageModel> {
    configured_model_with_output_bound(model, tools, MAX_OUTPUT_TOKENS)
}

fn configured_model_with_output_bound(
    model: Arc<plakous_test_support::ScriptedModel>,
    tools: &FrozenToolRegistry,
    max_output_tokens: u32,
) -> TestResult<ConfiguredLanguageModel> {
    let authority = runtime_authority(model.descriptor())?;
    let configuration = provider_configuration_with_output_bound(
        model.descriptor(),
        tools.definitions(),
        max_output_tokens,
    )?;
    Ok(ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        model,
        configuration,
        authority,
    )?)
}

fn user_item() -> TestResult<ConversationItem> {
    user_text_item(6, PROMPT)
}

fn request(
    descriptor: &ResolvedModelDescriptor,
    turn: u128,
    attempt: u128,
    history: Vec<ConversationItem>,
    tools: Vec<ToolDefinition>,
) -> TestResult<ModelRequest> {
    let configuration = provider_configuration(descriptor, &tools)?;
    Ok(ModelRequest::builder(
        configuration.selected_identity(),
        configuration.effective_identity(),
        ConversationView::from_items(history)?,
        RequestMetadata::for_turn(
            id::<RunId>(4)?,
            id::<TurnId>(turn)?,
            id::<AttemptId>(attempt)?,
            timestamp()?,
        ),
    )
    .tools(
        tools
            .into_iter()
            .map(|definition| {
                ConfiguredTool::new(
                    definition,
                    ToolExecutionDisposition::Local,
                    plakous_model::ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
    )
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .limits(generation_limits()?)
    .build()?)
}

fn segmented_request(
    descriptor: &ResolvedModelDescriptor,
    turn: u128,
    attempt: u128,
    segments: Vec<(HistorySegmentKind, Vec<ConversationItem>)>,
    tools: Vec<ToolDefinition>,
) -> TestResult<ModelRequest> {
    let configuration = provider_configuration(descriptor, &tools)?;
    let history =
        segments
            .into_iter()
            .try_fold(CanonicalHistory::default(), |history, (kind, items)| {
                Ok::<_, Box<dyn std::error::Error>>(
                    history.append(HistorySegment::new(kind, items)?)?,
                )
            })?;
    Ok(ModelRequest::builder(
        configuration.selected_identity(),
        configuration.effective_identity(),
        ConversationView::from_shared(history, WorkingProjection::default()),
        RequestMetadata::for_turn(
            id::<RunId>(4)?,
            id::<TurnId>(turn)?,
            id::<AttemptId>(attempt)?,
            timestamp()?,
        ),
    )
    .tools(
        tools
            .into_iter()
            .map(|definition| {
                ConfiguredTool::new(
                    definition,
                    ToolExecutionDisposition::Local,
                    plakous_model::ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
    )
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .limits(generation_limits()?)
    .build()?)
}

fn text_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    entry: u128,
    text: &str,
    start: plakous_test_support::ScenarioStart,
) -> TestResult<plakous_test_support::ModelScenario> {
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    let item = assistant_text_item(entry, text)?;
    let location = OutputLocation::new(0, 0);
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    Ok(plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
        start,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                location,
                BlockStart::Text {
                    phase: Some(TextPhase::Final),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::TextDelta(TextDelta::new(
                location,
                text.as_bytes(),
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(location),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ],
    ))
}

fn reasoning_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
) -> TestResult<plakous_test_support::ModelScenario> {
    const SUMMARY: &str = "bounded reasoning summary";
    const ANSWER: &str = "reasoned answer";
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    let item = ConversationItem::new(
        id::<EntryId>(100)?,
        Actor::Assistant,
        vec![
            ContentBlock::Reasoning(ReasoningBlock::new(
                ReasoningVisibility::Summary,
                Some(SUMMARY.to_owned()),
                None,
            )?),
            ContentBlock::Text(TextBlock::new(ANSWER, Some(TextPhase::Final))?),
        ],
        Provenance::ModelOutput,
    )?;
    let reasoning = OutputLocation::new(0, 0);
    let text = OutputLocation::new(0, 1);
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    Ok(plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
        plakous_test_support::ScenarioStart::Immediate,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                reasoning,
                BlockStart::Reasoning {
                    visibility: ReasoningVisibility::Summary,
                    replay: None,
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ReasoningDelta(
                ReasoningDelta::new(reasoning, SUMMARY.as_bytes()),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(reasoning),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                text,
                BlockStart::Text {
                    phase: Some(TextPhase::Final),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::TextDelta(TextDelta::new(
                text,
                ANSWER.as_bytes(),
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(text),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ],
    ))
}

fn empty_response_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
) -> TestResult<plakous_test_support::ModelScenario> {
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    Ok(plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
        plakous_test_support::ScenarioStart::Immediate,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(
                ModelResponse::new(
                    Vec::new(),
                    FinishDetails::new(FinishReason::Stop, None),
                    None,
                    None,
                    digest,
                ),
            )),
        ],
    ))
}

fn tool_definition(concurrency: ToolConcurrency) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Probe one value.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        concurrency,
        ToolEffect::ReadOnly,
    )?)
}

fn tool_batch_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    definition: &ToolDefinition,
) -> TestResult<(
    plakous_test_support::ModelScenario,
    ConversationItem,
    [ToolCallBlock; 2],
)> {
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    let calls = [
        ToolCallBlock::new(
            id::<CallId>(101)?,
            definition.name().clone(),
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            r#"{"value":"a"}"#,
        )?,
        ToolCallBlock::new(
            id::<CallId>(102)?,
            definition.name().clone(),
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            r#"{"value":"b"}"#,
        )?,
    ];
    let item = ConversationItem::new(
        id::<EntryId>(100)?,
        Actor::Assistant,
        calls.iter().cloned().map(ContentBlock::ToolCall).collect(),
        Provenance::ModelOutput,
    )?;
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::ToolCalls, None),
        None,
        None,
        digest,
    );
    let mut steps = vec![plakous_test_support::ScenarioStep::Emit(
        ModelEvent::ResponseStarted(ResponseStarted::new(None, digest)),
    )];
    for (block, call) in calls.iter().enumerate() {
        let location = OutputLocation::new(0, u32::try_from(block)?);
        steps.extend([
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                location,
                BlockStart::ToolCall {
                    call_id: call.call_id(),
                    tool: call.exposed_name().clone(),
                    owner: ToolExecutionOwner::Local,
                    state: ToolCallState::Requested,
                    correlation: ToolCorrelation::default(),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ToolInputDelta(
                ToolInputDelta::new(location, call.raw_arguments().as_bytes()),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(location),
            )),
        ]);
    }
    steps.push(plakous_test_support::ScenarioStep::Emit(
        ModelEvent::ResponseCompleted(response),
    ));
    Ok((
        plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
            plakous_test_support::ScenarioStart::Immediate,
            steps,
        ),
        item,
        calls,
    ))
}

fn canonical_tool_result(
    entry: u128,
    call: &ToolCallBlock,
    text: &str,
) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        id::<EntryId>(entry)?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock::new(
            call.call_id(),
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new(text, None)?)],
            call.correlation().clone(),
        )?)],
        Provenance::ToolExecution,
    )?)
}

#[derive(Default)]
struct ProbeGate {
    open: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

impl ProbeGate {
    fn open(&self) {
        if self.open.swap(true, Ordering::AcqRel) {
            return;
        }
        let wakers = {
            let mut wakers = self
                .wakers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    async fn wait(&self) {
        std::future::poll_fn(|context| {
            if self.open.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            let mut wakers = self
                .wakers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.open.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                if !wakers
                    .iter()
                    .any(|registered| registered.will_wake(context.waker()))
                {
                    wakers.push(context.waker().clone());
                }
                Poll::Pending
            }
        })
        .await;
    }
}

#[derive(Clone)]
struct ProbeCallControl {
    release: Arc<ProbeGate>,
    cleanup: Arc<ProbeGate>,
    result: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeEvent {
    Started(CallId),
    CancellationObserved(CallId),
    Finished(CallId),
}

struct ParallelProbeTool {
    calls: BTreeMap<CallId, ProbeCallControl>,
    events: mpsc::UnboundedSender<ProbeEvent>,
}

impl ParallelProbeTool {
    fn release(&self, call_id: CallId) -> TestResult {
        self.call(call_id)?.release.open();
        Ok(())
    }

    fn release_cleanup(&self, call_id: CallId) -> TestResult {
        self.call(call_id)?.cleanup.open();
        Ok(())
    }

    fn call(&self, call_id: CallId) -> TestResult<&ProbeCallControl> {
        self.calls
            .get(&call_id)
            .ok_or_else(|| "probe call control is unavailable".into())
    }
}

impl ToolExecutor for ParallelProbeTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        let call_id = invocation.call_id();
        let call = self.calls.get(&call_id).cloned();
        Box::pin(async move {
            let call = call.ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "probe received an unexpected call identity",
                )
            })?;
            let _ = self.events.send(ProbeEvent::Started(call_id));
            let result = tokio::select! {
                () = call.release.wait() => ToolOutcome::success_text(call.result),
                () = context.cancellation().cancelled() => {
                    let _ = self.events.send(ProbeEvent::CancellationObserved(call_id));
                    call.cleanup.wait().await;
                    Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Cancelled,
                        "probe cancelled after joined cleanup",
                    ))
                }
            };
            let _ = self.events.send(ProbeEvent::Finished(call_id));
            result
        })
    }
}

struct ToolBatchFixture {
    model: Arc<plakous_test_support::ScriptedModel>,
    registry: FrozenToolRegistry,
    tool: Arc<ParallelProbeTool>,
    events: mpsc::UnboundedReceiver<ProbeEvent>,
    calls: [CallId; 2],
    expected_results: [ConversationItem; 2],
}

fn tool_batch_fixture() -> TestResult<ToolBatchFixture> {
    let descriptor = descriptor(true, false)?;
    let definition = tool_definition(ToolConcurrency::Parallel)?;
    let first_request = request(
        &descriptor,
        5,
        7,
        vec![user_item()?],
        vec![definition.clone()],
    )?;
    let (first, assistant, calls) = tool_batch_scenario(&descriptor, first_request, &definition)?;
    let expected_results = [
        canonical_tool_result(8, &calls[0], "result-a")?,
        canonical_tool_result(9, &calls[1], "result-b")?,
    ];
    let second_request = segmented_request(
        &descriptor,
        5,
        11,
        vec![
            (HistorySegmentKind::RecentIntent, vec![user_item()?]),
            (
                HistorySegmentKind::ToolExchange,
                vec![
                    assistant,
                    expected_results[0].clone(),
                    expected_results[1].clone(),
                ],
            ),
        ],
        vec![definition.clone()],
    )?;
    let second = text_scenario(
        &descriptor,
        second_request,
        103,
        "batch-done",
        plakous_test_support::ScenarioStart::Immediate,
    )?;
    let model = Arc::new(scripted_model(descriptor, [first, second])?);
    let (sender, events) = mpsc::unbounded_channel();
    let controls = calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            (
                call.call_id(),
                ProbeCallControl {
                    release: Arc::new(ProbeGate::default()),
                    cleanup: Arc::new(ProbeGate::default()),
                    result: if index == 0 { "result-a" } else { "result-b" },
                },
            )
        })
        .collect();
    let tool = Arc::new(ParallelProbeTool {
        calls: controls,
        events: sender,
    });
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition.clone(),
        tool.clone(),
    ))?;
    Ok(ToolBatchFixture {
        model,
        registry: registry.freeze(),
        tool,
        events,
        calls: [calls[0].call_id(), calls[1].call_id()],
        expected_results,
    })
}

async fn next_probe_event(
    events: &mut mpsc::UnboundedReceiver<ProbeEvent>,
) -> TestResult<ProbeEvent> {
    events
        .recv()
        .await
        .ok_or_else(|| "parallel probe event channel closed".into())
}

async fn assert_both_started(
    events: &mut mpsc::UnboundedReceiver<ProbeEvent>,
    calls: [CallId; 2],
) -> TestResult {
    let mut started = Vec::with_capacity(2);
    for _ in 0..2 {
        let ProbeEvent::Started(call_id) = next_probe_event(events).await? else {
            return Err("probe finished before both calls started".into());
        };
        started.push(call_id);
    }
    started.sort_unstable();
    let mut expected = calls;
    expected.sort_unstable();
    assert_eq!(started, expected);
    Ok(())
}

fn tool_result_call_ids(items: &[ConversationItem]) -> Vec<CallId> {
    items
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.call_id()),
            _ => None,
        })
        .collect()
}

struct DecisionRequiredPolicy {
    revision: ToolPolicyRevision,
}

impl DecisionRequiredPolicy {
    fn new() -> TestResult<Self> {
        Ok(Self {
            revision: ToolPolicyRevision::new("test.require-decision", 1)?,
        })
    }
}

impl ToolPolicy for DecisionRequiredPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        self.revision.clone()
    }

    fn evaluate(&self, _request: ToolPolicyRequest) -> ToolPolicyFuture {
        Box::pin(async {
            Ok(ToolPolicyDecision::DecisionRequired(
                EffectDecisionPrompt::new("Allow the probe tool"),
            ))
        })
    }
}

async fn pending_decision_fixture() -> TestResult<(
    Agent,
    Turn,
    Arc<plakous_test_support::ScriptedTool>,
    TempDir,
)> {
    let descriptor = descriptor(true, false)?;
    let definition = tool_definition(ToolConcurrency::Serial)?;
    let first_request = request(
        &descriptor,
        5,
        7,
        vec![user_item()?],
        vec![definition.clone()],
    )?;
    let (scenario, _, _) = {
        let plan = descriptor.validate_request(first_request.clone(), ValidationMode::Strict)?;
        let digest = plan.digest();
        let call = ToolCallBlock::new(
            id::<CallId>(101)?,
            definition.name().clone(),
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            r#"{"value":"x"}"#,
        )?;
        let item = ConversationItem::new(
            id::<EntryId>(100)?,
            Actor::Assistant,
            vec![ContentBlock::ToolCall(call.clone())],
            Provenance::ModelOutput,
        )?;
        let location = OutputLocation::new(0, 0);
        let response = ModelResponse::new(
            vec![item.clone()],
            FinishDetails::new(FinishReason::ToolCalls, None),
            None,
            None,
            digest,
        );
        (
            plakous_test_support::ModelScenario::new(
                plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(
                    first_request,
                )),
                plakous_test_support::ScenarioStart::Immediate,
                vec![
                    plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                        ResponseStarted::new(None, digest),
                    )),
                    plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(
                        BlockStarted::new(
                            item.id(),
                            location,
                            BlockStart::ToolCall {
                                call_id: call.call_id(),
                                tool: call.exposed_name().clone(),
                                owner: ToolExecutionOwner::Local,
                                state: ToolCallState::Requested,
                                correlation: ToolCorrelation::default(),
                            },
                        ),
                    )),
                    plakous_test_support::ScenarioStep::Emit(ModelEvent::ToolInputDelta(
                        ToolInputDelta::new(location, call.raw_arguments().as_bytes()),
                    )),
                    plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                        BlockCompleted::new(location),
                    )),
                    plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(
                        response,
                    )),
                ],
            ),
            item,
            call,
        )
    };
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text("tool-ok")?)),
    ]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(model, registry.freeze(), workspace)?
        .tool_policy(Arc::new(DecisionRequiredPolicy::new()?))
        .identity_sources(clock, ids)
        .build()?;
    let turn = agent.prompt(PROMPT).await?;
    Ok((agent, turn, tool, directory))
}

async fn next_pending(events: &mut TurnEvents) -> TestResult<EffectDecisionRef> {
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::EffectDecisionRequired { decision, .. } => {
                return Ok(decision.clone());
            }
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
                return Err("turn terminalized before decision".into());
            }
            _ => {}
        }
    }
}

async fn next_resolution(events: &mut TurnEvents) -> TestResult<EffectDecisionResolution> {
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::EffectDecisionResolved { resolution, .. } => return Ok(*resolution),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
                return Err("turn terminalized before decision resolution".into());
            }
            _ => {}
        }
    }
}

fn assert_completed<'a>(result: &'a TurnResult, text: &str) -> TestResult<&'a ModelResponse> {
    let TurnResultOutcome::Completed(response) = result.outcome() else {
        return Err(format!(
            "expected completed outcome, got {:?}: {}",
            result.outcome(),
            result
                .error()
                .map_or("no error", plakous_agent::internal::AgentError::message)
        )
        .into());
    };
    assert_eq!(result.response(), Some(response));
    assert!(result.error().is_none());
    assert_eq!(response.output_text(), text);
    Ok(response)
}

fn assert_terminal_error(result: &TurnResult, expected: AgentErrorKind) -> TestResult {
    let error = result
        .error()
        .ok_or("non-completed result omitted its terminal error")?;
    assert_eq!(error.kind(), expected);
    assert!(result.response().is_none());
    Ok(())
}

async fn wait_for_dispatch(model: &plakous_test_support::ScriptedModel) -> TestResult {
    tokio::time::timeout(Duration::from_secs(1), async {
        while model.captured_plans().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn scripted_text_and_reasoning_close_with_authoritative_results() -> TestResult {
    for reasoning in [false, true] {
        let descriptor = descriptor(false, reasoning)?;
        let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
        let scenario = if reasoning {
            reasoning_scenario(&descriptor, expected)?
        } else {
            text_scenario(
                &descriptor,
                expected,
                100,
                "plain answer",
                plakous_test_support::ScenarioStart::Immediate,
            )?
        };
        let model = Arc::new(scripted_model(descriptor, [scenario])?);
        let (_directory, workspace) = workspace()?;
        let (clock, ids) = identity_sources()?;
        let agent = agent_builder(
            model.clone(),
            ToolRegistryBuilder::default().freeze(),
            workspace,
        )?
        .identity_sources(clock, ids)
        .build()?;

        let result = agent.prompt(PROMPT).await?.result().await?;
        let expected_text = if reasoning {
            "reasoned answer"
        } else {
            "plain answer"
        };
        assert_completed(&result, expected_text)?;
        assert_eq!(result.model_rounds(), 1);
        assert_eq!(result.tool_calls(), 0);
        assert_eq!(result.conversation().items().len(), 2);
        assert_eq!(result.checkpoint_id(), agent.checkpoint_id());
        let result_debug = format!("{result:?}");
        assert!(result_debug.contains("checkpoint_id"));
        assert!(result_debug.contains(&format!("{:?}", result.checkpoint_id())));
        assert!(!result_debug.contains(PROMPT));
        assert!(!result_debug.contains(expected_text));
        assert!(!result_debug.contains("RuntimeManifest"));
        assert!(!result_debug.contains("CanonicalHistory"));
        let agent_debug = format!("{agent:?}");
        assert!(agent_debug.contains(&format!("{:?}", agent.checkpoint_id())));
        assert!(!agent_debug.contains(PROMPT));
        assert!(!agent_debug.contains(expected_text));
        assert!(!agent_debug.contains("RuntimeManifest"));
        if reasoning {
            assert!(
                result.conversation().items()[1]
                    .blocks()
                    .iter()
                    .any(|block| matches!(
                        block,
                        ContentBlock::Reasoning(value)
                            if value.visibility() == ReasoningVisibility::Summary
                                && value.text() == Some("bounded reasoning summary")
                    ))
            );
        }
        assert_eq!(model.remaining_scenarios(), 0);
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn caller_command_identity_acknowledges_exact_duplicate_once() -> TestResult {
    let descriptor = descriptor(false, false)?;
    let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
    let scenario = text_scenario(
        &descriptor,
        expected,
        100,
        "one result",
        plakous_test_support::ScenarioStart::Immediate,
    )?;
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(
        model.clone(),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )?
    .identity_sources(clock, ids)
    .build()?;
    let command_id = CommandId::new("product-item:42")?;

    let accepted = agent.start(command_id.clone(), PROMPT).await?;
    assert_eq!(
        accepted.acknowledgement().disposition(),
        CommandAckDisposition::Accepted
    );
    let accepted_sequence = accepted.acknowledgement().sequence();
    let accepted_turn = accepted
        .acknowledgement()
        .turn_id()
        .ok_or("accepted start acknowledgement omitted its turn")?;
    let accepted_result = accepted
        .result()
        .ok_or("accepted start omitted its result handle")?
        .clone();
    let turn = accepted
        .into_turn()
        .ok_or("accepted start did not return its turn")?;

    let duplicate = agent.start(command_id.clone(), PROMPT).await?;
    assert_eq!(
        duplicate.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    assert_eq!(duplicate.acknowledgement().sequence(), accepted_sequence);
    assert_eq!(duplicate.acknowledgement().turn_id(), Some(accepted_turn));
    assert!(duplicate.turn().is_none());
    let duplicate_result = duplicate
        .into_result()
        .ok_or("active duplicate omitted its result handle")?;

    let conflict = agent
        .command(TurnCommand::Start {
            command_id: command_id.clone(),
            input: "different canonical content".to_owned().into(),
        })
        .await?;
    assert_eq!(
        conflict
            .acknowledgement()
            .rejection()
            .map(plakous_agent::internal::ControlError::kind),
        Some(ControlErrorKind::IdentityConflict)
    );
    assert!(conflict.result().is_none());

    let result = turn.result().await?;
    assert_eq!(result.turn_id(), accepted_turn);
    assert_completed(&result, "one result")?;
    for duplicate in [accepted_result, duplicate_result] {
        let duplicate = duplicate.result().await?;
        assert_eq!(duplicate.turn_id(), result.turn_id());
        assert_eq!(duplicate.checkpoint_id(), result.checkpoint_id());
        assert_eq!(duplicate.outcome(), result.outcome());
        assert_eq!(duplicate.usage(), result.usage());
        assert_eq!(duplicate.conversation(), result.conversation());
    }

    let completed_duplicate = agent.start(command_id, PROMPT).await?;
    assert!(completed_duplicate.turn().is_none());
    let completed_result = completed_duplicate
        .into_result()
        .ok_or("completed duplicate omitted its result handle")?
        .result()
        .await?;
    assert_eq!(completed_result.checkpoint_id(), result.checkpoint_id());
    assert_eq!(completed_result.outcome(), result.outcome());
    assert_eq!(model.captured_plans().len(), 1);
    agent.shutdown().await?;
    Ok(())
}

/// Asserts one tool lifecycle observation reports a caller the model issued itself.
fn assert_direct_lineage(lineage: &ToolCallerLineageView, label: &str) {
    assert_eq!(
        lineage.class(),
        ToolCallerClass::Direct,
        "{label} reported a programmatic caller class"
    );
    assert!(
        lineage.cell_id().is_none(),
        "{label} named an evaluator cell"
    );
    assert!(
        lineage.parent_call_id().is_none(),
        "{label} named an owning marker call"
    );
    assert!(
        lineage.nested_ordinal().is_none(),
        "{label} carried a nested ordinal"
    );
    assert!(lineage.depth().is_none(), "{label} carried a nested depth");
}

/// A parallel Direct tool round is one tool group and carries no lineage.
///
/// The presentation contract rejects the sequenced tool group as a presentation key precisely
/// because a parallel Direct round would otherwise read as one programmatic
/// cell. Every lifecycle observation here reports `Direct` with no cell, no
/// parent, no ordinal, and no depth, so nothing downstream can fold this round
/// into a batch.
#[tokio::test]
async fn parallel_direct_tool_round_carries_no_caller_lineage() -> TestResult {
    let ToolBatchFixture {
        model,
        registry,
        tool,
        events: mut probe_events,
        calls,
        expected_results: _,
    } = tool_batch_fixture()?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(model.clone(), registry, workspace)?
        .identity_sources(clock, ids)
        .build()?;
    let mut turn = agent.prompt(PROMPT).await?;
    let mut observations = turn.take_events()?;

    assert_both_started(&mut probe_events, calls).await?;
    tool.release(calls[1])?;
    tool.release(calls[0])?;

    let mut lifecycle = 0_usize;
    let mut completed = Vec::new();
    loop {
        let event = observations.recv().await?;
        if let Some(lineage) = event.kind().tool_caller_lineage() {
            lifecycle += 1;
            assert_direct_lineage(lineage, "parallel direct tool observation");
        }
        match event.kind() {
            AgentEventKind::ToolCompleted { .. } => {
                completed.push(event.call_id().ok_or("tool completion omitted call ID")?);
            }
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. } => {
                return Err("direct lineage turn emitted failure".into());
            }
            _ => {}
        }
    }
    assert_eq!(completed.len(), 2);
    assert!(
        lifecycle >= 4,
        "a two-tool round published {lifecycle} lifecycle observations"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn multiple_tools_finish_out_of_order_but_commit_in_source_order() -> TestResult {
    let ToolBatchFixture {
        model,
        registry,
        tool,
        events: mut probe_events,
        calls,
        expected_results,
    } = tool_batch_fixture()?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(model.clone(), registry, workspace)?
        .identity_sources(clock, ids)
        .build()?;
    let mut turn = agent.prompt(PROMPT).await?;
    let mut observations = turn.take_events()?;

    assert_both_started(&mut probe_events, calls).await?;
    tool.release(calls[1])?;
    assert_eq!(
        next_probe_event(&mut probe_events).await?,
        ProbeEvent::Finished(calls[1])
    );
    tool.release(calls[0])?;
    assert_eq!(
        next_probe_event(&mut probe_events).await?,
        ProbeEvent::Finished(calls[0])
    );

    let result = turn.result().await?;
    assert_completed(&result, "batch-done")?;
    assert_eq!(result.tool_calls(), 2);
    let conversation = result.conversation().items().to_vec();
    assert_eq!(&conversation[2..4], &expected_results);
    assert_eq!(tool_result_call_ids(&conversation), calls);
    let plans = model.captured_plans();
    assert_eq!(
        tool_result_call_ids(&plans[1].request().input().items().to_vec()),
        calls
    );
    assert!(
        plans[0]
            .request()
            .stable_prefix()
            .shares_storage_with(plans[1].request().stable_prefix())
    );
    assert!(
        agent
            .stable_request_prefix()
            .shares_storage_with(plans[0].request().stable_prefix())
    );
    assert_eq!(
        agent.stable_request_prefix().cache(),
        CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        }
    );
    assert!(!agent.harness_plan().allows_native_continuation());
    let mut completed = Vec::new();
    loop {
        let event = observations.recv().await?;
        match event.kind() {
            AgentEventKind::ToolCompleted { .. } => {
                completed.push(event.call_id().ok_or("tool completion omitted call ID")?);
            }
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. } => {
                return Err("successful multi-tool turn emitted failure".into());
            }
            _ => {}
        }
    }
    assert_eq!(completed, [calls[1], calls[0]]);
    assert_eq!(model.remaining_scenarios(), 0);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn length_stopped_tool_proposal_retries_once_without_dispatch_or_commit() -> TestResult {
    let (descriptor, definition, scenario) = unfinished_length_tool_scenario()?;
    let retry = any_request_scenario(&scenario);
    let model = Arc::new(scripted_model(descriptor, [scenario, retry])?);
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text(
            "must-not-run",
        )?)),
    ]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(model, registry.freeze(), workspace)?
        .identity_sources(clock, ids)
        .build()?;

    let result = agent.prompt(PROMPT).await?.result().await?;
    assert_terminal_error(&result, AgentErrorKind::Model)?;
    assert_eq!(
        tool.captured(),
        [] as [plakous_test_support::CapturedToolInvocation; 0]
    );
    let items = result.conversation().items();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0], user_item()?);
    assert!(matches!(
        items[1].blocks(),
        [ContentBlock::Text(text)]
            if text.text().contains("call was discarded and nothing ran")
                && text.text().contains("128 tokens")
    ));
    assert_failure_note(&items[2]);
    assert_eq!(result.checkpoint_id(), agent.checkpoint_id());
    agent.shutdown().await?;
    Ok(())
}

/// A Direct tool round carries `Direct` lineage on every lifecycle observation.
///
/// One serial tool that passes an operator decision publishes the complete
/// lifecycle — authorized, started, completed — and none of it names a cell, a
/// parent call, an ordinal, or a depth, so nothing downstream can present a
/// direct call as a programmatic cell.
#[tokio::test]
async fn direct_tool_lifecycle_reports_direct_caller_lineage() -> TestResult {
    let (agent, mut turn, tool, _workspace) = pending_decision_fixture().await?;
    let control = turn.control();
    let mut events = turn.take_events()?;
    let decision = next_pending(&mut events).await?;
    assert_eq!(
        control
            .allow_effect(
                decision,
                plakous_tools::EffectDecisionAuditId::new("test.direct-lineage")?,
            )
            .await?,
        EffectDecisionStatus::Allowed
    );

    let mut phases = Vec::new();
    loop {
        let event = events.recv().await?;
        if let Some(lineage) = event.kind().tool_caller_lineage() {
            assert_direct_lineage(lineage, "direct tool observation");
            phases.push(match event.kind() {
                AgentEventKind::ToolAuthorized { .. } => "authorized",
                AgentEventKind::ToolStarted { .. } => "started",
                AgentEventKind::ToolProgress { .. } => "progress",
                AgentEventKind::ToolCompleted { .. } => "completed",
                _ => "rejected",
            });
        }
        // The fixture scripts one model round, so the turn terminalises after
        // the tool result rather than continuing; either terminal ends the read.
        if matches!(
            event.kind(),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
        ) {
            break;
        }
    }
    assert!(phases.contains(&"authorized"), "phases: {phases:?}");
    assert!(phases.contains(&"started"), "phases: {phases:?}");
    assert!(phases.contains(&"completed"), "phases: {phases:?}");
    assert_eq!(tool.captured().len(), 1);
    let _ = turn.result().await;
    agent.shutdown().await?;
    Ok(())
}

/// The model-operation budget ends exploration, not the turn.
///
/// A model that spends the whole budget on tool calls used to get the user a
/// bare `RoundLimit` failure and no answer. The budget now buys one more round
/// with tool selection withdrawn on the wire, preceded by a visible note, and
/// the text that round produces completes the turn normally.
#[tokio::test]
async fn exhausted_operation_budget_answers_with_tool_selection_withdrawn() -> TestResult {
    budget_wrap_up_preserves_origin(false).await
}

#[tokio::test]
async fn wake_operation_budget_wrap_up_preserves_origin() -> TestResult {
    budget_wrap_up_preserves_origin(true).await
}

#[allow(
    clippy::too_many_lines,
    reason = "shared fixture verifies budget withdrawal, origin, events and retained history"
)]
async fn budget_wrap_up_preserves_origin(instruction: bool) -> TestResult {
    const ANSWER: &str = "what I found before the budget ran out";
    let descriptor = descriptor(true, false)?;
    let definition = tool_definition(ToolConcurrency::Serial)?;
    let first_request = request(
        &descriptor,
        5,
        7,
        vec![ConversationItem::new(
            id::<EntryId>(6)?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(PROMPT, None)?)],
            if instruction {
                Provenance::Instruction
            } else {
                Provenance::UserInput
            },
        )?],
        vec![definition.clone()],
    )?;
    let calling = single_tool_call_scenario(&descriptor, first_request.clone(), &definition, 100)?;
    let wrap_up = any_request_scenario(&text_scenario(
        &descriptor,
        first_request,
        103,
        ANSWER,
        plakous_test_support::ScenarioStart::Immediate,
    )?);
    let model = Arc::new(scripted_model(descriptor, [calling, wrap_up])?);
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text("polled")?)),
    ]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(Arc::clone(&model), registry.freeze(), workspace)?
        .identity_sources(clock, ids)
        .config(AgentConfig::default().with_max_model_rounds(1))
        .build()?;

    let mut turn = if instruction {
        agent
            .start_instruction(CommandId::new("wake-budget")?, PROMPT)
            .await?
            .into_turn()
            .ok_or("missing wake turn")?
    } else {
        agent.prompt(PROMPT).await?
    };
    // Canonical history is not what a surface reads: without this event the
    // user would get the wrap-up answer with no reason for it.
    assert!(
        budget_announced_before_terminal(&mut turn).await?,
        "the spent operation budget must be announced to surfaces"
    );
    let result = turn.result().await?;
    let TurnResultOutcome::Completed(response) = result.outcome() else {
        return Err(format!("budget wrap-up did not complete: {:?}", result.outcome()).into());
    };
    assert!(
        response_text(response).contains(ANSWER),
        "wrap-up answer was lost: {:?}",
        response.output()
    );
    assert_eq!(tool.captured().len(), 1);

    let plans = model.captured_plans();
    let origin = if instruction {
        plakous_model::TurnOrigin::HostWake
    } else {
        plakous_model::TurnOrigin::UserPrompt
    };
    assert!(
        plans
            .iter()
            .all(|plan| plan.request().turn_origin() == Some(origin))
    );
    assert_eq!(
        plans.len(),
        2,
        "expected the budgeted round and one wrap-up"
    );
    assert_eq!(plans[0].request().tool_choice(), &ToolChoice::Auto);
    assert_eq!(
        plans[1].request().tool_choice(),
        &ToolChoice::None,
        "the wrap-up round must withdraw tool selection on the wire"
    );
    assert!(
        !plans[1].request().tools().is_empty(),
        "the wrap-up round keeps the declared tools so the prompt prefix is stable"
    );
    assert!(
        request_text(plans[1].request())
            .contains(plakous_agent::internal::OPERATION_BUDGET_WRAP_UP_NOTE),
        "the wrap-up round must carry the visible budget note"
    );
    assert!(
        result
            .conversation()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(
                block,
                ContentBlock::Text(text)
                    if text.text() == plakous_agent::internal::OPERATION_BUDGET_WRAP_UP_NOTE
            )),
        "the transcript must show why the turn stopped"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn output_limit_retry_preserves_admitted_turn_origin() -> TestResult {
    for instruction in [false, true] {
        let descriptor = descriptor(false, false)?;
        let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
        let retry = plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::Any,
            plakous_test_support::ScenarioStart::Immediate,
            vec![plakous_test_support::ScenarioStep::Error(
                plakous_model::ModelError::new(
                    plakous_model::ModelErrorKind::OutputLimit,
                    "truncated response",
                ),
            )],
        );
        let answer = any_request_scenario(&text_scenario(
            &descriptor,
            expected,
            100,
            "done",
            plakous_test_support::ScenarioStart::Immediate,
        )?);
        let model = Arc::new(scripted_model(descriptor, [retry, answer])?);
        let (_directory, workspace) = workspace()?;
        let (clock, ids) = identity_sources()?;
        let agent = agent_builder(
            Arc::clone(&model),
            ToolRegistryBuilder::default().freeze(),
            workspace,
        )?
        .identity_sources(clock, ids)
        .build()?;
        let submission = if instruction {
            agent
                .start_instruction(CommandId::new("wake-retry")?, PROMPT)
                .await?
        } else {
            agent.start(CommandId::new("user-retry")?, PROMPT).await?
        };
        let result = submission
            .into_turn()
            .ok_or("missing turn")?
            .result()
            .await?;
        assert_completed(&result, "done")?;
        let plans = model.captured_plans();
        assert_eq!(plans.len(), 2);
        let origin = if instruction {
            plakous_model::TurnOrigin::HostWake
        } else {
            plakous_model::TurnOrigin::UserPrompt
        };
        assert!(
            plans
                .iter()
                .all(|plan| plan.request().turn_origin() == Some(origin))
        );
        // The retry note is part of the turn, not a turn of its own.
        let openings = plans
            .iter()
            .map(|plan| plan.request().turn_prompt().map(ConversationItem::id))
            .collect::<Vec<_>>();
        assert!(openings[0].is_some());
        assert_eq!(openings[1], openings[0]);
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn output_limit_retry_names_the_wire_maximum_not_the_product_bound() -> TestResult {
    const PRODUCT_BOUND: u32 = 128_000;
    const WIRE_LIMIT: u32 = 64_000;
    let descriptor = descriptor(false, false)?;
    let wire = NonZeroU32::new(WIRE_LIMIT).ok_or("nonzero wire limit")?;
    let retry = plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::Any,
        plakous_test_support::ScenarioStart::Immediate,
        vec![plakous_test_support::ScenarioStep::Error(
            plakous_model::ModelError::new(
                plakous_model::ModelErrorKind::OutputLimit,
                "truncated response",
            )
            .with_wire_output_limit(wire),
        )],
    );
    let answer = any_request_scenario(&text_scenario(
        &descriptor,
        request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?,
        100,
        "done",
        plakous_test_support::ScenarioStart::Immediate,
    )?);
    let model = Arc::new(scripted_model(descriptor, [retry, answer])?);
    let tools = ToolRegistryBuilder::default().freeze();
    let configured = configured_model_with_output_bound(Arc::clone(&model), &tools, PRODUCT_BOUND)?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = AgentBuilder::new(configured, tools, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(PRODUCT_BOUND), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(clock, ids)
        .build()?;
    let result = agent
        .start(CommandId::new("wire-limit-retry")?, PROMPT)
        .await?
        .into_turn()
        .ok_or("missing turn")?
        .result()
        .await?;
    assert_completed(&result, "done")?;
    let plans = model.captured_plans();
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0]
            .request()
            .limits()
            .max_output_tokens()
            .map(NonZeroU32::get),
        Some(PRODUCT_BOUND),
        "the product bound stays the request maximum"
    );
    let note = request_text(plans[1].request());
    assert!(
        note.contains("its output limit of 64000 tokens"),
        "the retry must name the wire maximum, not the product bound: {note}"
    );
    assert!(
        !note.contains("128000"),
        "the retry note must not name the product bound: {note}"
    );
    assert!(
        result
            .conversation()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(
                block,
                ContentBlock::Text(text) if text.text().contains("its output limit of 64000 tokens")
            )),
        "the transcript must show the wire maximum"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A provider `OutputLimit` that names no wire maximum is filled from the
/// prepared projection (`driver/model.rs`), not from the product bound.
#[tokio::test]
async fn output_limit_stream_error_fills_the_prepared_wire_limit() -> TestResult {
    const PREPARED: u32 = 64;
    let wire = NonZeroU32::new(PREPARED).ok_or("nonzero prepared wire limit")?;
    let descriptor = descriptor(false, false)?;
    let retry = plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::Any,
        plakous_test_support::ScenarioStart::Immediate,
        vec![plakous_test_support::ScenarioStep::Error(
            plakous_model::ModelError::new(
                plakous_model::ModelErrorKind::OutputLimit,
                "truncated response",
            ),
        )],
    );
    let answer = any_request_scenario(&text_scenario(
        &descriptor,
        request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?,
        100,
        "done",
        plakous_test_support::ScenarioStart::Immediate,
    )?);
    let model =
        Arc::new(scripted_model(descriptor, [retry, answer])?.with_prepared_output_limit(wire));
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(
        Arc::clone(&model),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )?
    .identity_sources(clock, ids)
    .build()?;
    let result = agent.prompt(PROMPT).await?.result().await?;
    assert_completed(&result, "done")?;
    let plans = model.captured_plans();
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0]
            .request()
            .limits()
            .max_output_tokens()
            .map(NonZeroU32::get),
        Some(MAX_OUTPUT_TOKENS),
        "the product bound stays the request maximum"
    );
    assert_prepared_wire_limits(&model, PREPARED);
    let note = request_text(plans[1].request());
    assert!(
        note.contains("its output limit of 64 tokens"),
        "the retry must name the prepared wire maximum, not the product bound: {note}"
    );
    assert!(
        !note.contains("128"),
        "the retry note must not name the product bound: {note}"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A length finish with an unfinished tool call names the prepared wire maximum
/// and persists it on the terminal model error (`turn.rs`).
#[tokio::test]
async fn length_stopped_tool_call_persists_the_prepared_wire_limit() -> TestResult {
    const PREPARED: u32 = 64;
    let wire = NonZeroU32::new(PREPARED).ok_or("nonzero prepared wire limit")?;
    let (descriptor, definition, scenario) = unfinished_length_tool_scenario()?;
    let retry = any_request_scenario(&scenario);
    let model =
        Arc::new(scripted_model(descriptor, [scenario, retry])?.with_prepared_output_limit(wire));
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text(
            "must-not-run",
        )?)),
    ]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(Arc::clone(&model), registry.freeze(), workspace)?
        .identity_sources(clock, ids)
        .build()?;
    let result = agent.prompt(PROMPT).await?.result().await?;
    assert_terminal_error(&result, AgentErrorKind::Model)?;
    assert_eq!(
        tool.captured(),
        [] as [plakous_test_support::CapturedToolInvocation; 0]
    );
    assert_eq!(
        model.captured_plans()[0]
            .request()
            .limits()
            .max_output_tokens()
            .map(NonZeroU32::get),
        Some(MAX_OUTPUT_TOKENS),
        "the product bound stays the request maximum"
    );
    assert_prepared_wire_limits(&model, PREPARED);
    let items = result.conversation().items();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0], user_item()?);
    assert!(matches!(
        items[1].blocks(),
        [ContentBlock::Text(text)]
            if text.text().contains("its output limit of 64 tokens")
                && text.text().contains("call was discarded and nothing ran")
                && !text.text().contains("128")
    ));
    assert_failure_note(&items[2]);
    let error = result.error().ok_or("terminal error")?;
    let model_error = error.model_error().ok_or("model error")?;
    assert_eq!(
        model_error.wire_output_limit().map(NonZeroU32::get),
        Some(PREPARED)
    );
    let encoded = serde_json::to_value(model_error)?;
    assert_eq!(encoded["wire_output_limit"], json!(PREPARED), "{encoded}");
    assert_eq!(
        serde_json::from_value::<plakous_model::ModelError>(encoded)?,
        *model_error
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn malformed_call_round_projects_terminal_model_error_phase() -> TestResult {
    let (descriptor, definition, scenario) = unfinished_tool_scenario(FinishReason::Stop)?;
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(model, registry.freeze(), workspace)?
        .identity_sources(clock, ids)
        .build()?;
    let result = agent.prompt(PROMPT).await?.result().await?;
    let error = result.error().ok_or("terminal model error")?;
    assert_eq!(error.kind(), AgentErrorKind::Model);
    assert_eq!(
        error.model_error().ok_or("model error")?.kind(),
        plakous_model::ModelErrorKind::IncompleteResponse
    );
    assert_eq!(
        error.model_context().ok_or("model error context")?.phase(),
        plakous_model::ModelErrorPhase::Terminal
    );
    assert_eq!(
        tool.captured(),
        [] as [plakous_test_support::CapturedToolInvocation; 0]
    );
    agent.shutdown().await?;
    Ok(())
}

fn unfinished_length_tool_scenario() -> TestResult<(
    ResolvedModelDescriptor,
    ToolDefinition,
    plakous_test_support::ModelScenario,
)> {
    unfinished_tool_scenario(FinishReason::Length)
}

fn unfinished_tool_scenario(
    finish: FinishReason,
) -> TestResult<(
    ResolvedModelDescriptor,
    ToolDefinition,
    plakous_test_support::ModelScenario,
)> {
    let descriptor = descriptor(true, false)?;
    let definition = tool_definition(ToolConcurrency::Serial)?;
    let expected = request(
        &descriptor,
        5,
        7,
        vec![user_item()?],
        vec![definition.clone()],
    )?;
    let plan = descriptor.validate_request(expected.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    let call = ToolCallBlock::new(
        id::<CallId>(101)?,
        definition.name().clone(),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        r#"{"value":"incomplete"}"#,
    )?;
    let assistant = ConversationItem::new(
        id::<EntryId>(100)?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(call.clone())],
        Provenance::ModelOutput,
    )?;
    let location = OutputLocation::new(0, 0);
    let response = ModelResponse::new(
        vec![assistant.clone()],
        FinishDetails::new(finish, None),
        None,
        None,
        digest,
    );
    let scenario = plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(expected)),
        plakous_test_support::ScenarioStart::Immediate,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                assistant.id(),
                location,
                BlockStart::ToolCall {
                    call_id: call.call_id(),
                    tool: call.exposed_name().clone(),
                    owner: ToolExecutionOwner::Local,
                    state: ToolCallState::Requested,
                    correlation: ToolCorrelation::default(),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ToolInputDelta(
                ToolInputDelta::new(location, call.raw_arguments().as_bytes()),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(location),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ],
    );
    Ok((descriptor, definition, scenario))
}

fn assert_prepared_wire_limits(model: &plakous_test_support::ScriptedModel, tokens: u32) {
    let prepared: Vec<_> = model
        .captured_records()
        .iter()
        .map(|record| {
            record
                .projection()
                .output_limit()
                .map(plakous_model::PreparedOutputLimit::tokens)
                .map(NonZeroU32::get)
        })
        .collect();
    assert_eq!(prepared, vec![Some(tokens), Some(tokens)]);
}

#[tokio::test]
async fn content_filter_during_wrap_up_keeps_one_budget_note_and_event() -> TestResult {
    const ANSWER: &str = "what I found before the budget ran out";
    let descriptor = descriptor(true, false)?;
    let definition = tool_definition(ToolConcurrency::Serial)?;
    let first_request = request(
        &descriptor,
        5,
        7,
        vec![user_item()?],
        vec![definition.clone()],
    )?;
    let calling = single_tool_call_scenario(&descriptor, first_request.clone(), &definition, 100)?;
    let wrap_up = any_request_scenario(&text_scenario(
        &descriptor,
        first_request,
        103,
        ANSWER,
        plakous_test_support::ScenarioStart::Immediate,
    )?);
    let filter = plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::Any,
        plakous_test_support::ScenarioStart::Immediate,
        vec![plakous_test_support::ScenarioStep::Error(
            plakous_model::ModelError::new(
                plakous_model::ModelErrorKind::ContentFilteredIncomplete,
                "filtered wrap-up",
            ),
        )],
    );
    let model = Arc::new(
        scripted_model(descriptor, [calling, filter, wrap_up])?.with_content_filter_instruction(
            "<content_filter_guidance>\nOffer another option.\n</content_filter_guidance>"
                .to_owned(),
        ),
    );
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text("polled")?)),
    ]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(Arc::clone(&model), registry.freeze(), workspace)?
        .identity_sources(clock, ids)
        .config(AgentConfig::default().with_max_model_rounds(1))
        .build()?;

    let mut turn = agent.prompt(PROMPT).await?;
    let mut events = turn.take_events()?;
    let mut budget_events = 0;
    loop {
        let event = events.recv().await?;
        if matches!(event.kind(), AgentEventKind::OperationBudgetExhausted) {
            budget_events += 1;
        }
        if matches!(
            event.kind(),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
        ) {
            break;
        }
    }
    assert_eq!(budget_events, 1);
    let result = turn.result().await?;
    assert_completed(&result, ANSWER)?;
    assert_eq!(tool.captured().len(), 1);

    let plans = model.captured_plans();
    assert_eq!(
        plans.len(),
        3,
        "expected exploration, filtered wrap-up and guided retry"
    );
    assert_eq!(plans[0].request().tool_choice(), &ToolChoice::Auto);
    assert_eq!(plans[2].request().tool_choice(), &ToolChoice::None);
    assert_eq!(plans[1].request().tool_choice(), &ToolChoice::None);
    assert!(
        result
            .conversation()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter(|block| matches!(
                block,
                ContentBlock::Text(text)
                    if text.text() == plakous_agent::internal::OPERATION_BUDGET_WRAP_UP_NOTE
            ))
            .count()
            == 1,
        "the transcript must have exactly one budget note"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A turn has no wall clock.
///
/// Nine model rounds each think for four virtual minutes, inside the
/// five-minute model-operation bound; the turn outlives the retired 20-minute
/// deadline and still completes with the model's answer. Only the model, the
/// round cap, cancellation, or a bounded per-operation failure ends a turn.
///
/// Time is advanced while each round is held at its start barrier rather than
/// inside a tool, because the tool runtime's own execution bound is measured
/// on `std::time::Instant`, which paused Tokio time does not move.
#[tokio::test(start_paused = true)]
async fn a_long_turn_completes_without_a_wall_clock() -> TestResult {
    const ANSWER: &str = "the build finished";
    const TOOL_ROUNDS: u128 = 8;
    const THINKING: Duration = Duration::from_secs(4 * 60);
    let descriptor = descriptor(true, false)?;
    let definition = tool_definition(ToolConcurrency::Serial)?;
    let first_request = request(
        &descriptor,
        5,
        7,
        vec![user_item()?],
        vec![definition.clone()],
    )?;
    let mut barriers = Vec::new();
    let mut scenarios = Vec::new();
    for round in 0..=TOOL_ROUNDS {
        let scenario = if round < TOOL_ROUNDS {
            single_tool_call_scenario(
                &descriptor,
                first_request.clone(),
                &definition,
                100 + round * 10,
            )?
        } else {
            text_scenario(
                &descriptor,
                first_request.clone(),
                200,
                ANSWER,
                plakous_test_support::ScenarioStart::Immediate,
            )?
        };
        let barrier = plakous_test_support::ScenarioBarrier::new();
        scenarios.push(plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::Any,
            plakous_test_support::ScenarioStart::Barrier(barrier.clone()),
            scenario.steps().to_vec(),
        ));
        barriers.push(barrier);
    }
    let model = Arc::new(scripted_model(descriptor, scenarios)?);
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps(
        (0..TOOL_ROUNDS).map(|_| {
            plakous_test_support::ScriptedToolStep::Complete(ToolOutcome::success_text("built"))
        }),
    ));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(Arc::clone(&model), registry.freeze(), workspace)?
        .identity_sources(clock, ids)
        .build()?;

    let started = tokio::time::Instant::now();
    let turn = agent.prompt(PROMPT).await?;
    for (round, barrier) in barriers.iter().enumerate() {
        while model.captured_plans().len() <= round {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(THINKING).await;
        barrier.release();
    }
    let result = turn.result().await?;
    let TurnResultOutcome::Completed(response) = result.outcome() else {
        return Err(format!("long turn did not complete: {:?}", result.outcome()).into());
    };
    assert!(response_text(response).contains(ANSWER));
    assert_eq!(tool.captured().len(), 8);
    assert!(started.elapsed() >= Duration::from_secs(36 * 60));
    agent.shutdown().await?;
    Ok(())
}

/// Whether the turn announced its spent operation budget before terminalising.
///
/// Drains to the turn terminal so the answer is never read before the
/// observation that explains it.
async fn budget_announced_before_terminal(turn: &mut Turn) -> TestResult<bool> {
    let mut events = turn.take_events()?;
    let mut announced = false;
    loop {
        let event = events.recv().await?;
        if matches!(event.kind(), AgentEventKind::OperationBudgetExhausted) {
            announced = true;
        }
        if matches!(
            event.kind(),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
        ) {
            return Ok(announced);
        }
    }
}

/// The same round-limited turn, with a model that ignores the withdrawal.
///
/// Nothing is dispatched and the turn fails with `RoundLimit`: the budget is
/// genuinely spent, and a wrap-up that asks for more tools cannot extend it.
#[tokio::test]
async fn wrap_up_round_that_still_calls_tools_fails_the_turn() -> TestResult {
    let descriptor = descriptor(true, false)?;
    let definition = tool_definition(ToolConcurrency::Serial)?;
    let first_request = request(
        &descriptor,
        5,
        7,
        vec![user_item()?],
        vec![definition.clone()],
    )?;
    let calling = single_tool_call_scenario(&descriptor, first_request.clone(), &definition, 100)?;
    let defiant = any_request_scenario(&single_tool_call_scenario(
        &descriptor,
        first_request,
        &definition,
        200,
    )?);
    let model = Arc::new(scripted_model(descriptor, [calling, defiant])?);
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text("polled")?)),
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text(
            "must-not-run",
        )?)),
    ]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool.clone(),
    ))?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(Arc::clone(&model), registry.freeze(), workspace)?
        .identity_sources(clock, ids)
        .config(AgentConfig::default().with_max_model_rounds(1))
        .build()?;

    let result = agent.prompt(PROMPT).await?.result().await?;
    assert_terminal_error(&result, AgentErrorKind::RoundLimit)?;
    assert_eq!(
        tool.captured().len(),
        1,
        "the defiant wrap-up round must not dispatch anything"
    );
    agent.shutdown().await?;
    Ok(())
}

/// One assistant round that requests exactly one local tool call.
fn single_tool_call_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    definition: &ToolDefinition,
    entry: u128,
) -> TestResult<plakous_test_support::ModelScenario> {
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    let call = ToolCallBlock::new(
        id::<CallId>(entry + 1)?,
        definition.name().clone(),
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        r#"{"value":"x"}"#,
    )?;
    let item = ConversationItem::new(
        id::<EntryId>(entry)?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(call.clone())],
        Provenance::ModelOutput,
    )?;
    let location = OutputLocation::new(0, 0);
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::ToolCalls, None),
        None,
        None,
        digest,
    );
    Ok(plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
        plakous_test_support::ScenarioStart::Immediate,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                location,
                BlockStart::ToolCall {
                    call_id: call.call_id(),
                    tool: call.exposed_name().clone(),
                    owner: ToolExecutionOwner::Local,
                    state: ToolCallState::Requested,
                    correlation: ToolCorrelation::default(),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ToolInputDelta(
                ToolInputDelta::new(location, call.raw_arguments().as_bytes()),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(location),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ],
    ))
}

/// The same scripted stream, accepting whatever request the runtime prepares.
///
/// The wrap-up round's exact input is the runtime's to decide: it carries an
/// injected note and a different tool-choice policy, so the test asserts on the
/// captured plan instead of pinning the request in advance.
fn any_request_scenario(
    scenario: &plakous_test_support::ModelScenario,
) -> plakous_test_support::ModelScenario {
    plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::Any,
        scenario.start().clone(),
        scenario.steps().to_vec(),
    )
}

#[tokio::test]
async fn content_filtered_sampling_retries_with_one_canonical_host_instruction() -> TestResult {
    use plakous_model::{ModelError, ModelErrorKind};
    use plakous_test_support::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep};
    let instruction =
        "<content_filter_guidance>\nOffer a permitted alternative.\n</content_filter_guidance>";
    for second_filter in [false, true] {
        let descriptor = descriptor(false, false)?;
        let filter = || {
            ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Error(ModelError::new(
                    ModelErrorKind::ContentFilteredIncomplete,
                    "stream disconnected before completion: Incomplete response returned, reason: content_filter",
                ))],
            )
        };
        let next = if second_filter {
            filter()
        } else {
            any_request_scenario(&text_scenario(
                &descriptor,
                request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?,
                100,
                "permitted answer",
                ScenarioStart::Immediate,
            )?)
        };
        let model = Arc::new(
            scripted_model(descriptor, [filter(), next])?
                .with_content_filter_instruction(instruction.to_owned()),
        );
        let (_directory, workspace) = workspace()?;
        let (clock, ids) = identity_sources()?;
        let agent = agent_builder(
            model.clone(),
            ToolRegistryBuilder::default().freeze(),
            workspace,
        )?
        .identity_sources(clock, ids)
        .build()?;
        let result = agent.prompt(PROMPT).await?.result().await?;
        if second_filter {
            let error = result.error().ok_or("second filter did not fail")?;
            assert_eq!(error.model_kind(), Some(ModelErrorKind::Policy));
            assert_eq!(
                error.message(),
                "stream disconnected before completion: Incomplete response returned, reason: content_filter"
            );
        } else {
            assert_completed(&result, "permitted answer")?;
        }
        let plans = model.captured_plans();
        assert_eq!(plans.len(), 2);
        let notes = plans[1]
            .request()
            .input()
            .items()
            .iter()
            .filter(|item| matches!(item.blocks(), [ContentBlock::Text(text)] if text.text() == instruction))
            .collect::<Vec<_>>();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].actor(), Actor::User);
        assert_eq!(notes[0].provenance(), Provenance::Instruction);
        assert!(
            matches!(notes[0].blocks(), [ContentBlock::Text(text)] if text.text() == instruction)
        );
        let notes = result
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(item.blocks(), [ContentBlock::Text(text)] if text.text() == instruction))
            .count();
        assert_eq!(notes, 1);
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn content_filter_retry_honors_delay_and_refuses_hosted_effect_risk() -> TestResult {
    use plakous_model::{
        ModelError, ModelErrorContext, ModelErrorKind, ModelErrorPhase, ModelRetryDelay,
        ModelRetryRisk, ModelRetryability,
    };
    use plakous_test_support::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep};
    let instruction =
        "<content_filter_guidance>\nOffer an alternative.\n</content_filter_guidance>";
    for retryability in [
        ModelRetryability::Eligible {
            retry_after: Some(ModelRetryDelay::from_millis(250)?),
        },
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        },
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        },
    ] {
        let descriptor = descriptor(false, false)?;
        let error = ModelError::contextual(
            ModelErrorKind::ContentFilteredIncomplete,
            "filtered",
            ModelErrorContext::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.requested_model().clone(),
                ModelErrorPhase::Terminal,
                retryability,
            )?,
        )?;
        let filter = ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Error(error)],
        );
        let answer = any_request_scenario(&text_scenario(
            &descriptor,
            request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?,
            100,
            "permitted answer",
            ScenarioStart::Immediate,
        )?);
        let model = Arc::new(
            scripted_model(descriptor, [filter, answer])?
                .with_content_filter_instruction(instruction.to_owned()),
        );
        let (_directory, workspace) = workspace()?;
        let (clock, ids) = identity_sources()?;
        let agent = agent_builder(
            model.clone(),
            ToolRegistryBuilder::default().freeze(),
            workspace,
        )?
        .identity_sources(clock, ids)
        .build()?;
        let started = tokio::time::Instant::now();
        let result = agent.prompt(PROMPT).await?.result().await?;
        let hosted = matches!(
            retryability,
            ModelRetryability::Unsafe {
                risk: ModelRetryRisk::ProviderHostedEffect
            }
        );
        if hosted {
            assert_terminal_error(&result, AgentErrorKind::OutcomeUnknown)?;
        } else {
            assert_completed(&result, "permitted answer")?;
        }
        if matches!(retryability, ModelRetryability::Eligible { .. }) {
            assert!(started.elapsed() >= Duration::from_millis(250));
        }
        assert_eq!(model.captured_plans().len(), if hosted { 1 } else { 2 });
        let guidance_count = result.conversation().items().iter()
            .filter(|item| matches!(item.blocks(), [ContentBlock::Text(text)] if text.text() == instruction)).count();
        assert_eq!(guidance_count, usize::from(!hosted));
        agent.shutdown().await?;
    }
    Ok(())
}

fn response_text(response: &ModelResponse) -> String {
    response
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect()
}

fn request_text(request: &ModelRequest) -> String {
    request
        .input()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn denial_is_model_visible_without_backend_dispatch() -> TestResult {
    let (agent, mut turn, tool, _workspace) = pending_decision_fixture().await?;
    let control = turn.control();
    let mut events = turn.take_events()?;
    let decision = next_pending(&mut events).await?;
    assert_eq!(
        control
            .deny_tool(
                decision,
                plakous_tools::ToolPolicyDenial::new("operator denied"),
            )
            .await?,
        EffectDecisionStatus::Denied
    );
    assert_eq!(
        next_resolution(&mut events).await?,
        EffectDecisionResolution::Denied
    );
    assert_eq!(
        tool.captured(),
        [] as [plakous_test_support::CapturedToolInvocation; 0]
    );

    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_terminal_error(&result, AgentErrorKind::Model)?;
    let denied = result
        .conversation()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].kind(), ToolResultKind::Failure);
    // The failed turn's prompt is not left dangling: a typed harness note
    // naming only the error class follows the committed exchange.
    let note = result
        .conversation()
        .items()
        .iter()
        .last()
        .ok_or("failed turn committed no history")?;
    assert_failure_note(note);
    while let Ok(event) = events.recv().await {
        assert!(!matches!(event.kind(), AgentEventKind::ToolStarted { .. }));
    }
    agent.shutdown().await?;
    Ok(())
}

/// The harness-authored note a cancelled turn appends after its prompt.
fn assert_cancelled_prompt_note(item: &ConversationItem) {
    assert_eq!(item.actor(), Actor::User);
    assert_eq!(item.provenance(), Provenance::Instruction);
    assert!(
        matches!(item.blocks(), [ContentBlock::Text(text)]
            if text.text().contains("cancelled the request above")),
        "{item:?}"
    );
}

#[test]
fn cancellation_at_model_tool_and_decision_phases_is_joined() -> TestResult {
    // Poll the cancellation chain on the same stack budget as product hosts.
    let result = std::thread::Builder::new()
        .stack_size(plakous_agent::internal::RUNTIME_EXECUTOR_STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?
                .block_on(cancellation_phase_matrix())
                .map_err(|error| error.to_string())
        })?
        .join()
        .map_err(|_| "cancellation scenario thread panicked")?;
    result.map_err(Into::into)
}

#[allow(
    clippy::too_many_lines,
    reason = "one cancellation matrix keeps model, tool, and decision phase invariants together"
)]
async fn cancellation_phase_matrix() -> TestResult {
    {
        let descriptor = descriptor(false, false)?;
        let barrier = plakous_test_support::ScenarioBarrier::new();
        let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
        let scenario = text_scenario(
            &descriptor,
            expected,
            100,
            "must not commit",
            plakous_test_support::ScenarioStart::Barrier(barrier.clone()),
        )?;
        let model = Arc::new(scripted_model(descriptor, [scenario])?);
        let (_directory, workspace) = workspace()?;
        let (clock, ids) = identity_sources()?;
        let agent = agent_builder(
            model.clone(),
            ToolRegistryBuilder::default().freeze(),
            workspace,
        )?
        .identity_sources(clock, ids)
        .build()?;
        let turn = agent.prompt(PROMPT).await?;
        let control = turn.control();
        wait_for_dispatch(model.as_ref()).await?;
        assert!(matches!(
            control.cancel().await?,
            CancellationStatus::Requested | CancellationStatus::AlreadyTerminal
        ));
        let result = turn.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
        assert_terminal_error(&result, AgentErrorKind::Cancelled)?;
        // The prompt stays; a harness note withdraws it so a later turn does
        // not answer it as if it were still pending.
        let items = result.conversation().items().to_vec();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], user_item()?);
        assert_cancelled_prompt_note(&items[1]);
        barrier.release();
        agent.shutdown().await?;
    }

    {
        let ToolBatchFixture {
            model,
            registry,
            tool,
            events: mut probe_events,
            calls,
            ..
        } = tool_batch_fixture()?;
        let (_directory, workspace) = workspace()?;
        let (clock, ids) = identity_sources()?;
        let agent = agent_builder(model.clone(), registry, workspace)?
            .identity_sources(clock, ids)
            .build()?;
        let turn = agent.prompt(PROMPT).await?;
        let control = turn.control();
        assert_both_started(&mut probe_events, calls).await?;
        let cancellation = tokio::spawn(async move { control.cancel().await });
        let mut cancelled = Vec::new();
        for _ in 0..2 {
            let ProbeEvent::CancellationObserved(call_id) =
                next_probe_event(&mut probe_events).await?
            else {
                return Err("tool completed before observing cancellation".into());
            };
            cancelled.push(call_id);
        }
        cancelled.sort_unstable();
        let mut expected = calls;
        expected.sort_unstable();
        assert_eq!(cancelled, expected);
        assert!(!cancellation.is_finished());
        tool.release_cleanup(calls[1])?;
        assert_eq!(
            next_probe_event(&mut probe_events).await?,
            ProbeEvent::Finished(calls[1])
        );
        assert!(!cancellation.is_finished());
        tool.release_cleanup(calls[0])?;
        assert_eq!(
            next_probe_event(&mut probe_events).await?,
            ProbeEvent::Finished(calls[0])
        );
        assert!(matches!(
            cancellation.await??,
            CancellationStatus::Requested | CancellationStatus::AlreadyTerminal
        ));
        let result = turn.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
        assert_terminal_error(&result, AgentErrorKind::Cancelled)?;
        let items = result.conversation().items().to_vec();
        assert_eq!(items.first(), Some(&user_item()?));
        assert_eq!(items.len(), 5);
        assert!(matches!(
            items[1].blocks(),
            [ContentBlock::ToolCall(_), ContentBlock::ToolCall(_)]
        ));
        assert!(items[2..4].iter().all(|item| matches!(item.blocks(),
            [ContentBlock::ToolResult(result)] if result.kind() == ToolResultKind::Failure)));
        assert_cancelled_prompt_note(&items[4]);
        assert_eq!(model.captured_plans().len(), 1);
        agent.shutdown().await?;
    }

    {
        let (agent, mut turn, tool, _workspace) = pending_decision_fixture().await?;
        let control = turn.control();
        let mut events = turn.take_events()?;
        let _decision = next_pending(&mut events).await?;
        assert!(matches!(
            control.cancel().await?,
            CancellationStatus::Requested | CancellationStatus::AlreadyTerminal
        ));
        assert_eq!(
            next_resolution(&mut events).await?,
            EffectDecisionResolution::Cancelled
        );
        assert_eq!(
            tool.captured(),
            [] as [plakous_test_support::CapturedToolInvocation; 0]
        );
        let result = turn.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
        assert_terminal_error(&result, AgentErrorKind::Cancelled)?;
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one deterministic schedule proves saturation, queued cancellation, capacity reuse, and joined shutdown"
)]
async fn fifo_saturation_queued_cancel_and_shutdown_are_ghost_free() -> TestResult {
    let descriptor = descriptor(false, false)?;
    let barrier = plakous_test_support::ScenarioBarrier::new();
    let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
    let scenario = text_scenario(
        &descriptor,
        expected,
        100,
        "must not complete",
        plakous_test_support::ScenarioStart::Barrier(barrier.clone()),
    )?;
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(
        model.clone(),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )?
    .config(AgentConfig::default().with_prompt_queue_capacity(2))
    .identity_sources(clock, ids)
    .build()
    .map_err(|error| format!("saturation agent build failed: {}", error.message()))?;
    let active = agent
        .prompt(PROMPT)
        .await
        .map_err(|error| format!("active admission failed: {}", error.message()))?;
    wait_for_dispatch(model.as_ref())
        .await
        .map_err(|error| format!("active dispatch wait failed: {error}"))?;
    let queued_one = agent
        .prompt("queued-one")
        .await
        .map_err(|error| format!("first queued admission failed: {}", error.message()))?;
    let queued_two = agent
        .prompt("queued-two")
        .await
        .map_err(|error| format!("second queued admission failed: {}", error.message()))?;
    let queued_two_id = queued_two.id();
    let retry_id = CommandId::new("retry-after-queue-full")?;
    let refused = agent
        .command(TurnCommand::Start {
            command_id: retry_id.clone(),
            input: "replacement".into(),
        })
        .await?;
    assert_eq!(
        refused
            .acknowledgement()
            .rejection()
            .ok_or("missing refusal")?
            .kind(),
        ControlErrorKind::QueueFull
    );
    assert!(!refused.acknowledgement().is_accepted());
    let queued_cancel = tokio::time::timeout(Duration::from_secs(2), queued_two.control().cancel())
        .await
        .map_err(|_| "queued cancellation did not settle its control capability")?
        .map_err(|error| format!("queued cancellation control failed: {error}"))?;
    assert!(matches!(
        queued_cancel,
        CancellationStatus::Requested | CancellationStatus::AlreadyTerminal
    ));
    let queued_two_result = tokio::time::timeout(Duration::from_secs(2), queued_two.result())
        .await
        .map_err(|_| "queued cancellation did not terminalize its exact root")?
        .map_err(|error| format!("queued cancellation result failed: {}", error.message()))?;
    assert_eq!(queued_two_result.turn_id(), queued_two_id);
    assert!(matches!(
        queued_two_result.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    let replacement = tokio::time::timeout(
        Duration::from_secs(2),
        agent.start(retry_id.clone(), "replacement"),
    )
    .await
    .map_err(|_| "queued cancellation did not release its exact FIFO capacity")?
    .map_err(|error| {
        format!(
            "replacement admission after queued cancellation failed: {}",
            error.message()
        )
    })?;

    assert_eq!(
        replacement.acknowledgement().disposition(),
        CommandAckDisposition::Accepted
    );
    let duplicate = agent
        .command(TurnCommand::Start {
            command_id: retry_id,
            input: "replacement".into(),
        })
        .await?;
    assert!(duplicate.acknowledgement().is_accepted());
    assert_eq!(
        duplicate.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    let replacement = replacement.into_turn().ok_or("missing replacement turn")?;

    tokio::time::timeout(Duration::from_secs(2), agent.shutdown())
        .await
        .map_err(|_| "shutdown did not cancel the saturated active/queued workload")?
        .map_err(|error| format!("joined shutdown failed: {}", error.message()))?;
    barrier.release();
    let active_result = active
        .result()
        .await
        .map_err(|error| format!("active cancellation result failed: {}", error.message()))?;
    let queued_one_result = queued_one
        .result()
        .await
        .map_err(|error| format!("queued cancellation result failed: {}", error.message()))?;
    let replacement_result = replacement.result().await.map_err(|error| {
        format!(
            "replacement cancellation result failed: {}",
            error.message()
        )
    })?;
    for result in [active_result, queued_one_result, replacement_result] {
        assert!(matches!(result.outcome(), TurnResultOutcome::Cancelled(_)));
        assert_terminal_error(&result, AgentErrorKind::Cancelled)?;
    }
    assert_eq!(model.captured_plans().len(), 1);
    assert_eq!(
        agent
            .prompt("after-shutdown")
            .await
            .err()
            .ok_or("joined shutdown admitted a prompt")?
            .kind(),
        AgentErrorKind::Shutdown
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_of_recorded_rejection_is_never_an_accepted_command() -> TestResult {
    let agent = text_agent("unused")?;
    let id = CommandId::new("invalid-root-command")?;
    for disposition in [
        CommandAckDisposition::Rejected,
        CommandAckDisposition::Duplicate,
    ] {
        let submission = agent
            .command(TurnCommand::Start {
                command_id: id.clone(),
                input: String::new().into(),
            })
            .await?;
        assert_eq!(submission.acknowledgement().disposition(), disposition);
        assert!(!submission.acknowledgement().is_accepted());
        assert_eq!(
            submission
                .acknowledgement()
                .rejection()
                .ok_or("missing rejection")?
                .kind(),
            ControlErrorKind::InvalidInput
        );
        assert!(submission.result().is_none());
        assert!(submission.into_turn().is_none());
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn image_prompt_on_a_model_without_image_input_is_refused_naming_the_route() -> TestResult {
    let agent = text_agent("unused")?;
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&2_u32.to_be_bytes());
    png.extend_from_slice(&3_u32.to_be_bytes());
    let input = plakous_agent::UserInput::new(vec![
        plakous_agent::UserPart::Text("what is this".into()),
        plakous_agent::UserPart::Image {
            media_type: "image/png".into(),
            bytes: png,
        },
    ]);
    let submission = agent
        .command(TurnCommand::Start {
            command_id: CommandId::new("image-root")?,
            input,
        })
        .await?;
    let rejection = submission
        .acknowledgement()
        .rejection()
        .ok_or("image prompt was admitted without ImageInput")?;
    assert_eq!(rejection.kind(), ControlErrorKind::InvalidInput);
    assert!(rejection.message().contains("does not accept image input"));
    assert!(rejection.message().contains("route "));
    assert!(submission.into_turn().is_none());
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn settled_command_retention_crosses_the_ledger_bound_without_wedging() -> TestResult {
    let agent = text_agent("one root")?;
    let root_id = CommandId::new("retained-root")?;
    let root = agent
        .start(root_id.clone(), PROMPT)
        .await?
        .into_turn()
        .ok_or("missing turn")?;
    root.result().await?;
    for index in 0..4_100 {
        let submission = agent
            .command(TurnCommand::Start {
                command_id: CommandId::new(format!("settled-invalid:{index}"))?,
                input: String::new().into(),
            })
            .await?;
        assert_eq!(
            submission
                .acknowledgement()
                .rejection()
                .ok_or("rejection")?
                .kind(),
            ControlErrorKind::InvalidInput
        );
    }
    let recent = agent
        .command(TurnCommand::Start {
            command_id: CommandId::new("settled-invalid:4099")?,
            input: String::new().into(),
        })
        .await?;
    assert_eq!(
        recent.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    let duplicate = agent.start(root_id, PROMPT).await?;
    assert_eq!(
        duplicate.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    assert!(duplicate.acknowledgement().is_accepted());
    duplicate
        .into_result()
        .ok_or("retained result")?
        .result()
        .await?;
    agent.shutdown().await?;
    Ok(())
}

fn text_agent(text: &'static str) -> TestResult<Agent> {
    let descriptor = descriptor(false, false)?;
    let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
    let scenario = text_scenario(
        &descriptor,
        expected,
        100,
        text,
        plakous_test_support::ScenarioStart::Immediate,
    )?;
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    Ok(
        agent_builder(model, ToolRegistryBuilder::default().freeze(), workspace)?
            .config(AgentConfig::default().with_event_capacity(32))
            .identity_sources(clock, ids)
            .build()?,
    )
}

#[derive(Default)]
struct OriginSink(Mutex<Vec<plakous_model::TurnOrigin>>);

#[tokio::test]
async fn every_agent_delivers_retry_logging_even_after_analytics_closes() -> TestResult {
    #[derive(Default)]
    struct RetrySink(Mutex<Vec<(plakous_agent::TelemetryContext, plakous_model::ModelRetry)>>);
    impl plakous_agent::TelemetrySink for RetrySink {
        fn try_emit(
            &self,
            _: plakous_agent::TelemetryObservation,
        ) -> Result<(), plakous_agent::TelemetrySinkError> {
            Err(plakous_agent::TelemetrySinkError::Closed)
        }
        fn observe_model_retry(
            &self,
            context: plakous_agent::TelemetryContext,
            retry: &plakous_model::ModelRetry,
        ) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((context, retry.clone()));
        }
    }
    let descriptor = descriptor(false, false)?;
    let item = ConversationItem::new(
        id::<EntryId>(6)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(PROMPT, None)?)],
        Provenance::UserInput,
    )?;
    let expected = request(&descriptor, 5, 7, vec![item], Vec::new())?;
    let scenario = text_scenario(
        &descriptor,
        expected,
        100,
        "done",
        plakous_test_support::ScenarioStart::Immediate,
    )?;
    let mut steps = scenario.steps().to_vec();
    let retry = plakous_model::ModelRetry::new(
        2,
        4,
        Duration::ZERO,
        plakous_model::ModelRetryCause::RecordRejectedInTransit,
        false,
        "peer_rejected_record",
    )
    .with_request("fixture.invalid", "/messages", 123);
    steps.insert(
        0,
        plakous_test_support::ScenarioStep::Emit(ModelEvent::Retry(retry.clone())),
    );
    let scenario = plakous_test_support::ModelScenario::new(
        scenario.expected().clone(),
        scenario.start().clone(),
        steps,
    );
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let sink = Arc::new(RetrySink::default());
    let agent = agent_builder(model, ToolRegistryBuilder::default().freeze(), workspace)?
        .identity_sources(clock, ids)
        .telemetry(Arc::new(plakous_agent::TelemetryEmitter::enabled(
            sink.clone(),
        )))
        .build()?;
    let turn = agent.prompt(PROMPT).await?;
    let turn_id = turn.id();
    turn.result().await?;
    agent.shutdown().await?;
    let observations = sink.0.lock().map_err(|_| "retry sink poisoned")?;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].0.turn_id(), Some(turn_id));
    assert_eq!(observations[0].1, retry);
    Ok(())
}

impl plakous_agent::TelemetrySink for OriginSink {
    fn try_emit(
        &self,
        observation: plakous_agent::TelemetryObservation,
    ) -> Result<(), plakous_agent::TelemetrySinkError> {
        if let plakous_agent::TelemetryObservationKind::TurnStarted { origin, .. } =
            observation.kind()
        {
            self.0
                .lock()
                .map_err(|_| plakous_agent::TelemetrySinkError::Closed)?
                .push(origin);
        }
        Ok(())
    }
}

#[tokio::test]
async fn turn_start_telemetry_distinguishes_user_prompt_from_host_wake() -> TestResult {
    for (instruction, origin) in [
        (false, plakous_model::TurnOrigin::UserPrompt),
        (true, plakous_model::TurnOrigin::HostWake),
    ] {
        let descriptor = descriptor(false, false)?;
        let item = ConversationItem::new(
            id::<EntryId>(6)?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(PROMPT, None)?)],
            if instruction {
                Provenance::Instruction
            } else {
                Provenance::UserInput
            },
        )?;
        let expected = request(&descriptor, 5, 7, vec![item], Vec::new())?;
        assert_eq!(expected.turn_origin(), Some(origin));
        let scenario = text_scenario(
            &descriptor,
            expected,
            100,
            "done",
            plakous_test_support::ScenarioStart::Immediate,
        )?;
        let model = Arc::new(scripted_model(descriptor, [scenario])?);
        let (_directory, workspace) = workspace()?;
        let (clock, ids) = identity_sources()?;
        let sink = Arc::new(OriginSink::default());
        let agent = agent_builder(model, ToolRegistryBuilder::default().freeze(), workspace)?
            .identity_sources(clock, ids)
            .telemetry(Arc::new(plakous_agent::TelemetryEmitter::enabled(
                sink.clone(),
            )))
            .build()?;
        let command_id = CommandId::new("origin-test")?;
        let submission = if instruction {
            agent.start_instruction(command_id, PROMPT).await?
        } else {
            agent.start(command_id, PROMPT).await?
        };
        submission
            .into_turn()
            .ok_or("missing turn")?
            .result()
            .await?;
        agent.shutdown().await?;
        assert_eq!(
            *sink.0.lock().map_err(|_| "origin sink poisoned")?,
            vec![origin]
        );
    }
    Ok(())
}

#[tokio::test]
async fn result_before_event_drain_and_event_before_result_are_equivalent() -> TestResult {
    {
        let agent = text_agent("result-first")?;
        let mut turn = agent.prompt(PROMPT).await?;
        let mut events = turn.take_events()?;
        let turn_id = turn.id();
        let result = turn.result().await?;
        assert_eq!(result.turn_id(), turn_id);
        assert_completed(&result, "result-first")?;
        let terminal = loop {
            let event = events.recv().await?;
            if matches!(event.kind(), AgentEventKind::TurnCompleted { .. }) {
                break event;
            }
        };
        assert_eq!(terminal.turn_id(), Some(turn_id));
        agent.shutdown().await?;
    }

    {
        let agent = text_agent("event-first")?;
        let mut turn = agent.prompt(PROMPT).await?;
        let mut events = turn.take_events()?;
        let turn_id = turn.id();
        let result = tokio::spawn(async move { turn.result().await });
        loop {
            let event = events.recv().await?;
            if matches!(event.kind(), AgentEventKind::TurnCompleted { .. }) {
                assert_eq!(event.turn_id(), Some(turn_id));
                break;
            }
        }
        let result = result.await??;
        assert_eq!(result.turn_id(), turn_id);
        assert_completed(&result, "event-first")?;
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn subscriber_loss_and_saturation_do_not_block_result_authority() -> TestResult {
    let descriptor = descriptor(false, false)?;
    let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
    let scenario = text_scenario(
        &descriptor,
        expected,
        100,
        "subscriber-independent",
        plakous_test_support::ScenarioStart::Immediate,
    )?;
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(model, ToolRegistryBuilder::default().freeze(), workspace)?
        .config(
            AgentConfig::default()
                .with_max_event_subscribers(2)
                .with_event_capacity(1),
        )
        .identity_sources(clock, ids)
        .build()?;
    let tiny = SubscriptionLimits::new(1, 64 * 1024)?;
    let slow = agent
        .attach(ViewRequest::new(EventVisibility::Metadata, tiny))
        .await?;
    let slow_cursor = slow.snapshot().cursor();
    let (_, mut slow_events) = slow.into_parts();
    let saturated = agent.attach(ViewRequest::metadata()).await?;
    assert!(agent.attach(ViewRequest::metadata()).await.is_err());

    let result = agent.prompt(PROMPT).await?.result().await?;
    assert_completed(&result, "subscriber-independent")?;
    assert!(matches!(
        slow_events.recv().await,
        Err(SubscriptionEnd::SubscriberLagged { last_delivered, .. })
            if last_delivered == slow_cursor
    ));
    drop(saturated);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn malformed_terminal_response_is_one_closed_failed_result() -> TestResult {
    let descriptor = descriptor(false, false)?;
    let expected = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
    let scenario = empty_response_scenario(&descriptor, expected)?;
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(model, ToolRegistryBuilder::default().freeze(), workspace)?
        .identity_sources(clock, ids)
        .build()?;

    let result = agent.prompt(PROMPT).await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_terminal_error(&result, AgentErrorKind::Model)?;
    let items = result.conversation().items();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], user_item()?);
    assert_failure_note(&items[1]);
    agent.shutdown().await?;
    Ok(())
}

fn assert_failure_note(note: &ConversationItem) {
    assert_eq!(note.actor(), Actor::User);
    assert_eq!(note.provenance(), Provenance::Instruction);
    assert!(
        matches!(note.blocks(), [ContentBlock::Text(text)]
            if text.text().starts_with("the turn handling the request above failed (")),
        "{note:?}"
    );
}

#[tokio::test]
async fn steering_queue_is_fifo_and_bounded_at_safe_boundaries() -> TestResult {
    let descriptor = descriptor(false, false)?;
    let first_barrier = plakous_test_support::ScenarioBarrier::new();
    let first_request = request(&descriptor, 5, 7, vec![user_item()?], Vec::new())?;
    let first = text_scenario(
        &descriptor,
        first_request,
        100,
        "first",
        plakous_test_support::ScenarioStart::Barrier(first_barrier.clone()),
    )?;
    let second_request = segmented_request(
        &descriptor,
        5,
        10,
        vec![
            (HistorySegmentKind::RecentIntent, vec![user_item()?]),
            (
                HistorySegmentKind::Conversation,
                vec![assistant_text_item(100, "first")?],
            ),
            (
                HistorySegmentKind::Steering,
                vec![user_text_item(8, "steer-a")?, user_text_item(9, "steer-b")?],
            ),
        ],
        Vec::new(),
    )?;
    let second = text_scenario(
        &descriptor,
        second_request,
        101,
        "second",
        plakous_test_support::ScenarioStart::Immediate,
    )?;
    let model = Arc::new(scripted_model(descriptor, [first, second])?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(
        model.clone(),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )?
    .config(AgentConfig::default().with_steering_queue_capacity(2))
    .identity_sources(clock, ids)
    .build()?;
    let turn = agent.prompt(PROMPT).await?;
    let control = turn.control();
    wait_for_dispatch(model.as_ref()).await?;
    assert_eq!(
        control.steer("steer-a").await?,
        SteeringStatus::Queued { position: 1 }
    );
    assert_eq!(
        control.steer("steer-b").await?,
        SteeringStatus::Queued { position: 2 }
    );
    let overflow = control
        .steer("steer-c")
        .await
        .err()
        .ok_or("steering queue admitted overflow")?;
    assert_eq!(overflow.kind(), ControlErrorKind::QueueFull);
    first_barrier.release();

    let result = turn.result().await?;
    assert_completed(&result, "second")?;
    let captured = model.captured_plans();
    assert_eq!(captured.len(), 2);
    let users = captured[1]
        .request()
        .input()
        .items()
        .iter()
        .filter(|item| item.actor() == Actor::User)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(users, [PROMPT, "steer-a", "steer-b"]);
    agent.shutdown().await?;
    Ok(())
}
