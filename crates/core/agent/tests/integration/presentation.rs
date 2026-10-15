//! Test-only presentation consumers over the public agent surface.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use plakous_agent::internal::{
    AgentEvent, AgentEventKind, AgentRequestPolicy, AgentViewReducer, AgentViewSnapshot,
    AgentViewState, EventId, EventSubscription, EventVisibility, NormalizedModelEventKind,
    TurnResult, TurnViewState, ViewRequest,
};
use plakous_model::{
    AttemptId, BlockCompleted, BlockStart, BlockStarted, CachePolicy, Capability,
    CompiledProviderConfiguration, ConfiguredLanguageModel, ConversationItem, ConversationView,
    FinishDetails, FinishReason, FixedClock, ModelEvent, ModelId, ModelProviderRequestId,
    ModelRequest, ModelResponse, ModelResponseMetadata, OutputBoundDisposition, OutputLocation,
    ReasoningPolicy, RequestMetadata, ResolvedModelDescriptor, ResponseStarted, RunId,
    SequenceIdGenerator, TextDelta, TextPhase, TurnId, ValidationMode,
};
use plakous_tools::ToolRegistryBuilder;
use serde_json::json;

use crate::support::{
    self, DescriptorIds, Fixture, MAX_OUTPUT_TOKENS, TestResult, assistant_text_item,
    generation_limits, id, timestamp, workspace,
};

const PROMPT: &str = "golden prompt";
const RESPONSE: &str = "golden response";
const ACTUAL_MODEL: &str = "provider.actual";
const REQUEST_ID: &str = "presentation-request-id-sentinel";
const FIXTURE: Fixture = Fixture {
    label: "presentation",
    provider_semantics: b"presentation-semantics-v1",
};
const DESCRIPTOR: DescriptorIds = DescriptorIds {
    protocol: "test.protocol",
    route: "test.route",
    model: "test.model",
    family: "test.presentation",
    profile: "test.profile",
};

fn runtime_authority(
    descriptor: &ResolvedModelDescriptor,
) -> TestResult<plakous_model::RuntimeModelAuthority> {
    support::runtime_authority(&FIXTURE, descriptor)
}

fn provider_configuration(
    descriptor: &ResolvedModelDescriptor,
) -> TestResult<CompiledProviderConfiguration> {
    support::provider_configuration(
        &FIXTURE,
        descriptor,
        &runtime_authority(descriptor)?,
        &[],
        CachePolicy::Disabled,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(MAX_OUTPUT_TOKENS).ok_or("nonzero presentation output bound")?,
        ),
    )
}

fn descriptor() -> TestResult<ResolvedModelDescriptor> {
    support::descriptor(&FIXTURE, &DESCRIPTOR, &[Capability::Streaming])
}

fn identity_sources() -> TestResult<(Arc<FixedClock>, Arc<SequenceIdGenerator>)> {
    support::identity_sources(32)
}

fn scripted_model(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = plakous_test_support::ModelScenario>,
) -> TestResult<plakous_test_support::ScriptedModel> {
    support::scripted_model(&FIXTURE, descriptor, scenarios)
}

#[derive(Default)]
struct LineEventPrinter {
    lines: Vec<String>,
    event_sequences: HashMap<EventId, u64>,
}

impl LineEventPrinter {
    fn push(&mut self, event: &AgentEvent) -> TestResult {
        let sequence = event.branch_sequence().get();
        let parent = match event.causal_parent() {
            Some(event_id) => self
                .event_sequences
                .get(&event_id)
                .ok_or("a projected causal parent did not precede its child")?
                .to_string(),
            None => "-".to_owned(),
        };
        let detail = match event.kind() {
            AgentEventKind::Model {
                kind: NormalizedModelEventKind::TextDelta,
                ..
            } => {
                let text = match event_text(event.kind()) {
                    Some(text) => serde_json::to_string(&text)?,
                    None => "<redacted>".to_owned(),
                };
                format!(" text={text}")
            }
            AgentEventKind::ConversationCommitted {
                item_count, items, ..
            } => {
                let text = match items
                    .as_deref()
                    .and_then(|items| items.iter().find_map(|item| item.display_text()))
                {
                    Some(text) => serde_json::to_string(text)?,
                    None => "<redacted>".to_owned(),
                };
                format!(" items={item_count} text={text}")
            }
            _ => String::new(),
        };
        self.lines.push(format!(
            "{sequence} {}{detail} parent={parent}",
            event_label(event.kind())
        ));
        self.event_sequences.insert(event.event_id(), sequence);
        Ok(())
    }

    fn output(&self) -> String {
        terminated_lines(&self.lines)
    }
}

#[derive(Default)]
struct VersionedJsonlTap {
    lines: Vec<String>,
    event_sequences: HashMap<EventId, u64>,
}

impl VersionedJsonlTap {
    fn push_event(&mut self, event: &AgentEvent) -> TestResult {
        let sequence = event.branch_sequence().get();
        let parent = match event.causal_parent() {
            Some(event_id) => Some(
                *self
                    .event_sequences
                    .get(&event_id)
                    .ok_or("a projected causal parent did not precede its child")?,
            ),
            None => None,
        };
        let record = json!({
            "event": {
                "kind_version": event.event_kind_version(),
                "schema": event.event_schema(),
            },
            "kind": event_label(event.kind()),
            "parent": parent,
            "projection": event.projection_revision(),
            "record": "event",
            "seq": sequence,
            "text": event_text(event.kind()),
            "v": 1,
            "visibility": visibility_label(event.visibility()),
        });
        self.lines.push(serde_json::to_string(&record)?);
        self.event_sequences.insert(event.event_id(), sequence);
        Ok(())
    }

    fn push_result(&mut self, result: &TurnResult) -> TestResult {
        let response = result
            .response()
            .ok_or("presentation success omitted its completed response")?;
        let record = json!({
            "conversation_items": result.conversation().items().len(),
            "finish": format!("{:?}", response.finish().reason()).to_ascii_lowercase(),
            "output": response.output_text(),
            "record": "result",
            "rounds": result.model_rounds(),
            "tool_calls": result.tool_calls(),
            "v": 1,
        });
        self.lines.push(serde_json::to_string(&record)?);
        Ok(())
    }

    fn push_snapshot(&mut self, snapshot: &AgentViewSnapshot) -> TestResult {
        let conversation = snapshot.conversation().map(|items| {
            items
                .iter()
                .filter_map(|item| item.display_text().map(str::to_owned))
                .collect::<Vec<_>>()
        });
        let turns = snapshot
            .turns()
            .iter()
            .map(|turn| {
                json!({
                    "model_attempts": turn.model_attempts(),
                    "state": turn_state_label(turn.state()),
                    "tools_completed": turn.tools_completed(),
                })
            })
            .collect::<Vec<_>>();
        let record = json!({
            "committed_items": snapshot.committed_conversation_items(),
            "conversation": conversation,
            "incomplete": snapshot.presentation_incomplete(),
            "record": "snapshot",
            "seq": snapshot.cursor().sequence().get(),
            "state": agent_state_label(snapshot.state()),
            "turns": turns,
            "v": 1,
            "visibility": visibility_label(snapshot.cursor().visibility()),
        });
        self.lines.push(serde_json::to_string(&record)?);
        Ok(())
    }

    fn output(&self) -> String {
        terminated_lines(&self.lines)
    }
}

struct PresentationOutput {
    printer: LineEventPrinter,
    tap: VersionedJsonlTap,
    snapshot: AgentViewSnapshot,
    events: Vec<Arc<AgentEvent>>,
}

async fn consume_turn(
    initial: AgentViewSnapshot,
    mut events: EventSubscription,
    turn_id: TurnId,
) -> TestResult<PresentationOutput> {
    let mut reducer = AgentViewReducer::from_snapshot(initial);
    let mut printer = LineEventPrinter::default();
    let mut tap = VersionedJsonlTap::default();
    let mut captured = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await??;
        assert_eq!(event.turn_id(), Some(turn_id));
        printer.push(event.as_ref())?;
        tap.push_event(event.as_ref())?;
        reducer.apply(event.as_ref())?;
        captured.push(Arc::clone(&event));
        match event.kind() {
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. } => {
                return Err("the deterministic presentation turn failed".into());
            }
            _ => {}
        }
    }
    Ok(PresentationOutput {
        printer,
        tap,
        snapshot: reducer.into_snapshot(),
        events: captured,
    })
}

#[tokio::test]
async fn public_line_printer_and_jsonl_tap_are_causal_versioned_and_redacted() -> TestResult {
    let descriptor = descriptor()?;
    let configuration = provider_configuration(&descriptor)?;
    let expected = request(&configuration, vec![user_item()?], 7)?;
    let scenario = text_scenario(&descriptor, expected, 100, RESPONSE)?;
    let model = Arc::new(scripted_model(descriptor, [scenario])?);
    let authority = runtime_authority(model.descriptor())?;
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        model,
        configuration,
        authority,
    )?;
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = plakous_agent::internal::AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )
    .request_policy(AgentRequestPolicy::new(
        generation_limits()?,
        ReasoningPolicy::default(),
    ))
    .identity_sources(clock, ids)
    .build()?;

    let (content_snapshot, content_events) =
        agent.attach(ViewRequest::content()).await?.into_parts();
    let (metadata_snapshot, metadata_events) =
        agent.attach(ViewRequest::metadata()).await?.into_parts();
    let turn = agent.prompt(PROMPT).await?;
    let turn_id = turn.id();
    let result = turn.result().await?;

    let mut content = consume_turn(content_snapshot, content_events, turn_id).await?;
    content.tap.push_result(&result)?;
    content.tap.push_snapshot(&content.snapshot)?;
    let mut metadata = consume_turn(metadata_snapshot, metadata_events, turn_id).await?;
    metadata.tap.push_snapshot(&metadata.snapshot)?;

    assert!(content.printer.output().contains("model_dispatch_started"));
    assert_eq!(content.printer.output(), GOLDEN_CONTENT_LINES);
    assert_eq!(metadata.printer.output(), GOLDEN_METADATA_LINES);
    assert_eq!(content.tap.output(), GOLDEN_CONTENT_JSONL);
    assert_eq!(metadata.tap.output(), GOLDEN_METADATA_JSONL);
    assert!(!metadata.tap.output().contains(PROMPT));
    assert!(!metadata.tap.output().contains(RESPONSE));
    assert!(metadata.snapshot.conversation().is_none());
    assert_eq!(
        result.actual_model().map(ModelId::as_str),
        Some(ACTUAL_MODEL)
    );
    assert_eq!(result.provider_request_id(), Some(REQUEST_ID));
    let started = content
        .events
        .iter()
        .find_map(|event| match event.kind().model_event() {
            Some(ModelEvent::ResponseStarted(started)) => Some(started),
            _ => None,
        })
        .ok_or("content presentation omitted response-start metadata")?;
    assert_eq!(
        started.actual_model().map(ModelId::as_str),
        Some(ACTUAL_MODEL)
    );
    assert_eq!(started.provider_request_id(), Some(REQUEST_ID));
    assert!(
        metadata
            .events
            .iter()
            .filter(|event| matches!(event.kind(), AgentEventKind::Model { .. }))
            .all(|event| event.kind().model_event().is_none())
    );
    let presentation = format!(
        "{}{}{:?}{result:?}",
        content.printer.output(),
        content.tap.output(),
        content.events
    );
    assert!(!presentation.contains(REQUEST_ID));
    assert!(presentation.contains("<redacted>"));

    let fresh_content = agent.attach(ViewRequest::content()).await?;
    assert_eq!(&content.snapshot, fresh_content.snapshot());
    let fresh_metadata = agent.attach(ViewRequest::metadata()).await?;
    assert_eq!(&metadata.snapshot, fresh_metadata.snapshot());

    agent.shutdown().await?;
    Ok(())
}

fn event_text(kind: &AgentEventKind) -> Option<String> {
    match kind {
        AgentEventKind::Model {
            event: Some(event), ..
        } => match event.as_ref() {
            ModelEvent::TextDelta(delta) => {
                Some(String::from_utf8_lossy(delta.bytes()).into_owned())
            }
            _ => None,
        },
        AgentEventKind::ConversationCommitted {
            items: Some(items), ..
        } => {
            let text = items
                .iter()
                .filter_map(|item| item.display_text())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn event_label(kind: &AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::AgentClosed => "agent_closed",
        AgentEventKind::TurnStarted => "turn_started",
        AgentEventKind::ModelStarted => "model_started",
        AgentEventKind::ModelDispatchStarted => "model_dispatch_started",
        AgentEventKind::RequestPrepared { .. } => "request_prepared",
        AgentEventKind::Model { kind, .. } => model_event_label(*kind),
        AgentEventKind::EffectDecisionRequired { .. } => "tool_decision_required",
        AgentEventKind::EffectDecisionResolved { .. } => "tool_decision_resolved",
        AgentEventKind::ToolAuthorized { .. } => "tool_authorized",
        AgentEventKind::ToolStarted { .. } => "tool_started",
        AgentEventKind::ToolProgress { .. } => "tool_progress",
        AgentEventKind::ToolPreparationRejected { .. } => "tool_preparation_rejected",
        AgentEventKind::ToolCompleted { .. } => "tool_completed",
        AgentEventKind::SteeringQueued { .. } => "steering_queued",
        AgentEventKind::SteeringApplied => "steering_applied",
        AgentEventKind::ConversationCommitted { .. } => "conversation_committed",
        AgentEventKind::CompactionStarted { .. } => "compaction_started",
        AgentEventKind::CompactionFinished { .. } => "compaction_finished",
        AgentEventKind::TurnCompleted { .. } => "turn_completed",
        AgentEventKind::TurnFailed { .. } => "turn_failed",
        _ => "unknown",
    }
}

fn model_event_label(kind: NormalizedModelEventKind) -> &'static str {
    match kind {
        NormalizedModelEventKind::ResponseStarted => "model.response_started",
        NormalizedModelEventKind::BlockStarted => "model.block_started",
        NormalizedModelEventKind::TextDelta => "model.text_delta",
        NormalizedModelEventKind::ReasoningDelta => "model.reasoning_delta",
        NormalizedModelEventKind::ReasoningReplayBound => "model.reasoning_replay_bound",
        NormalizedModelEventKind::ReasoningVisibilityBound => "model.reasoning_visibility_bound",
        NormalizedModelEventKind::ToolInputDelta => "model.tool_input_delta",
        NormalizedModelEventKind::CitationAdded => "model.citation_added",
        NormalizedModelEventKind::BlockCompleted => "model.block_completed",
        NormalizedModelEventKind::Usage => "model.usage",
        NormalizedModelEventKind::ResponseCompleted => "model.response_completed",
        NormalizedModelEventKind::ProviderEvent => "model.provider_event",
        _ => "model.unknown",
    }
}

fn visibility_label(visibility: EventVisibility) -> &'static str {
    match visibility {
        EventVisibility::Metadata => "metadata",
        EventVisibility::Content => "content",
        _ => "unknown",
    }
}

fn agent_state_label(state: AgentViewState) -> &'static str {
    match state {
        AgentViewState::Running => "running",
        AgentViewState::Closed => "closed",
        _ => "unknown",
    }
}

fn turn_state_label(state: TurnViewState) -> &'static str {
    match state {
        TurnViewState::Running => "running",
        TurnViewState::Completed => "completed",
        TurnViewState::Failed => "failed",
        _ => "unknown",
    }
}

fn terminated_lines(lines: &[String]) -> String {
    let mut output = lines.join("\n");
    output.push('\n');
    output
}

fn user_item() -> TestResult<ConversationItem> {
    support::user_text_item(6, PROMPT)
}

fn request(
    configuration: &CompiledProviderConfiguration,
    history: Vec<ConversationItem>,
    attempt: u128,
) -> TestResult<ModelRequest> {
    Ok(ModelRequest::builder(
        configuration.selected_identity(),
        configuration.effective_identity(),
        ConversationView::new(history),
        RequestMetadata::for_turn(
            id::<RunId>(4)?,
            id::<TurnId>(5)?,
            id::<AttemptId>(attempt)?,
            timestamp()?,
        ),
    )
    .limits(generation_limits()?)
    .build()?)
}

fn text_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    entry: u128,
    text: &str,
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
    )
    .with_metadata(ModelResponseMetadata::new(
        Some(ModelId::new(ACTUAL_MODEL)?),
        Some(ModelProviderRequestId::new(REQUEST_ID)?),
    ));
    Ok(plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
        plakous_test_support::ScenarioStart::Immediate,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest).with_metadata(ModelResponseMetadata::new(
                    Some(ModelId::new(ACTUAL_MODEL)?),
                    Some(ModelProviderRequestId::new(REQUEST_ID)?),
                )),
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

const GOLDEN_CONTENT_LINES: &str = concat!(
    "1 turn_started parent=-\n",
    "2 model_started parent=1\n",
    "3 request_prepared parent=2\n",
    "4 model_dispatch_started parent=3\n",
    "5 model.response_started parent=4\n",
    "6 model.block_started parent=5\n",
    "7 model.text_delta text=\"golden response\" parent=6\n",
    "8 model.block_completed parent=7\n",
    "9 model.response_completed parent=8\n",
    "10 turn_completed parent=9\n",
);

const GOLDEN_METADATA_LINES: &str = concat!(
    "1 turn_started parent=-\n",
    "2 model_started parent=1\n",
    "3 request_prepared parent=2\n",
    "4 model_dispatch_started parent=3\n",
    "5 model.response_started parent=4\n",
    "6 model.block_started parent=5\n",
    "7 model.text_delta text=<redacted> parent=6\n",
    "8 model.block_completed parent=7\n",
    "9 model.response_completed parent=8\n",
    "10 turn_completed parent=9\n",
);

const GOLDEN_CONTENT_JSONL: &str = concat!(
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"turn_started\",\"parent\":null,\"projection\":6,\"record\":\"event\",\"seq\":1,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model_started\",\"parent\":1,\"projection\":6,\"record\":\"event\",\"seq\":2,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"request_prepared\",\"parent\":2,\"projection\":6,\"record\":\"event\",\"seq\":3,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model_dispatch_started\",\"parent\":3,\"projection\":6,\"record\":\"event\",\"seq\":4,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.response_started\",\"parent\":4,\"projection\":6,\"record\":\"event\",\"seq\":5,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.block_started\",\"parent\":5,\"projection\":6,\"record\":\"event\",\"seq\":6,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.text_delta\",\"parent\":6,\"projection\":6,\"record\":\"event\",\"seq\":7,\"text\":\"golden response\",\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.block_completed\",\"parent\":7,\"projection\":6,\"record\":\"event\",\"seq\":8,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.response_completed\",\"parent\":8,\"projection\":6,\"record\":\"event\",\"seq\":9,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"turn_completed\",\"parent\":9,\"projection\":6,\"record\":\"event\",\"seq\":10,\"text\":null,\"v\":1,\"visibility\":\"content\"}\n",
    "{\"conversation_items\":2,\"finish\":\"stop\",\"output\":\"golden response\",\"record\":\"result\",\"rounds\":1,\"tool_calls\":0,\"v\":1}\n",
    "{\"committed_items\":0,\"conversation\":[],\"incomplete\":false,\"record\":\"snapshot\",\"seq\":10,\"state\":\"running\",\"turns\":[{\"model_attempts\":1,\"state\":\"completed\",\"tools_completed\":0}],\"v\":1,\"visibility\":\"content\"}\n",
);

const GOLDEN_METADATA_JSONL: &str = concat!(
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"turn_started\",\"parent\":null,\"projection\":6,\"record\":\"event\",\"seq\":1,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model_started\",\"parent\":1,\"projection\":6,\"record\":\"event\",\"seq\":2,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"request_prepared\",\"parent\":2,\"projection\":6,\"record\":\"event\",\"seq\":3,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model_dispatch_started\",\"parent\":3,\"projection\":6,\"record\":\"event\",\"seq\":4,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.response_started\",\"parent\":4,\"projection\":6,\"record\":\"event\",\"seq\":5,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.block_started\",\"parent\":5,\"projection\":6,\"record\":\"event\",\"seq\":6,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.text_delta\",\"parent\":6,\"projection\":6,\"record\":\"event\",\"seq\":7,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.block_completed\",\"parent\":7,\"projection\":6,\"record\":\"event\",\"seq\":8,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"model.response_completed\",\"parent\":8,\"projection\":6,\"record\":\"event\",\"seq\":9,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"event\":{\"kind_version\":11,\"schema\":3},\"kind\":\"turn_completed\",\"parent\":9,\"projection\":6,\"record\":\"event\",\"seq\":10,\"text\":null,\"v\":1,\"visibility\":\"metadata\"}\n",
    "{\"committed_items\":0,\"conversation\":null,\"incomplete\":false,\"record\":\"snapshot\",\"seq\":10,\"state\":\"running\",\"turns\":[{\"model_attempts\":1,\"state\":\"completed\",\"tools_completed\":0}],\"v\":1,\"visibility\":\"metadata\"}\n",
);
