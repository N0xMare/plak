//! K0 test-only presentation consumers over the public agent surface.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use plakous_agent::internal::{
    AgentEvent, AgentEventKind, AgentRequestPolicy, AgentViewReducer, AgentViewSnapshot,
    AgentViewState, EventId, EventSubscription, EventVisibility, NormalizedModelEventKind,
    TurnResult, TurnViewState, ViewRequest,
};
use plakous_model::{
    Actor, AttemptId, BlockCompleted, BlockStart, BlockStarted, CachePolicy, Capability,
    CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule,
    CompiledProviderConfiguration, ConfiguredLanguageModel, ContentBlock,
    ContextAdmissionRequirement, ContextContract, ContextEvidenceSource, ConversationItem,
    ConversationView, CountCertificateKind, CountObservation, CredentialSelectionId,
    EffectiveProviderConfiguration, EntryId, EvidenceFreshness, FactProvenance, FactSource,
    FamilyPolicyEnvelope, FeatureSupport, FinishDetails, FinishReason, FixedClock,
    GenerationLimits, LocalDurabilityDisposition, MeasurementCertificate, ModelAdapterProfileId,
    ModelEvent, ModelFamilyId, ModelId, ModelImplementationIdentity, ModelProviderRequestId,
    ModelRequest, ModelResponse, ModelResponseMetadata, OutputBoundDisposition, OutputLocation,
    OutputPolicy, ParallelToolDisposition, ProtocolId, Provenance, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderSemanticEvidence, ProviderStorageDisposition, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningPolicy, ReasoningSummaryDisposition, ReplayDisposition,
    RequestMetadata, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, ResponseStarted,
    ResponseStateDisposition, RetentionDisposition, RouteId, RunId, RuntimeModelAuthority,
    SafeLimit, SelectedProviderConfiguration, SequenceIdGenerator, Support, TextBlock, TextDelta,
    TextPhase, Timestamp, ToolChoice, TurnId, ValidationMode,
};
use plakous_tools::{ToolRegistryBuilder, WorkspacePolicy};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const PROMPT: &str = "golden prompt";
const RESPONSE: &str = "golden response";
const ACTUAL_MODEL: &str = "provider.actual";
const REQUEST_ID: &str = "presentation-request-id-sentinel";
const MAX_OUTPUT_TOKENS: u32 = 128;
const CONTEXT_COUNTER_ID: &str = "presentation-scripted-counter";
const CONTEXT_COUNTER_REVISION: &str = "presentation-scripted-exact-v1";

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

fn id<T>(value: u128) -> TestResult<T>
where
    T: TryFrom<Uuid>,
    T::Error: std::error::Error + 'static,
{
    Ok(T::try_from(Uuid::from_u128(value))?)
}

fn timestamp() -> TestResult<Timestamp> {
    Ok(Timestamp::new(1, 0)?)
}

fn context_contract(protocol: ProtocolId) -> TestResult<ContextContract> {
    let freshness = EvidenceFreshness::new(timestamp()?, None)?;
    Ok(ContextContract::new(
        protocol,
        "scripted-accounting-v1",
        "scripted-projection-v1",
        "scripted-reservation-v1",
        CapacityCertificate::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "presentation-scripted-capacity",
            "presentation-scripted-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000_000_000),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            CONTEXT_COUNTER_ID,
            CONTEXT_COUNTER_REVISION,
            "presentation-scripted-complete-projection",
            "scripted-lowering-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )?)
}

fn context_observation() -> TestResult<CountObservation> {
    Ok(CountObservation::new(
        CONTEXT_COUNTER_ID,
        CONTEXT_COUNTER_REVISION,
        64,
    )?)
}

fn generation_limits() -> TestResult<GenerationLimits> {
    Ok(GenerationLimits::new(
        NonZeroU32::new(MAX_OUTPUT_TOKENS),
        Vec::new(),
    )?)
}

fn descriptor() -> TestResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, timestamp()?, None);
    let features = BTreeMap::from([(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    )]);
    let protocol = ProtocolId::new("test.protocol")?;
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        protocol.clone(),
        RouteId::new("test.route")?,
        ModelId::new("test.model")?,
        None,
        Some(ModelFamilyId::new("test.presentation")?),
        ModelAdapterProfileId::new("test.profile")?,
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(context_contract(protocol)?))
}

fn scripted_model(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = plakous_test_support::ModelScenario>,
) -> TestResult<plakous_test_support::ScriptedModel> {
    let authority = runtime_authority(&descriptor)?;
    Ok(
        plakous_test_support::ScriptedModel::new(descriptor, scenarios)
            .with_runtime_authority(authority)
            .with_context_observation(context_observation()?),
    )
}

fn runtime_authority(descriptor: &ResolvedModelDescriptor) -> TestResult<RuntimeModelAuthority> {
    let account = plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ModelBinding,
        1,
        &[b"presentation-scripted-account"],
    )?;
    let credential = plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ModelBinding,
        1,
        &[b"presentation-scripted-credential"],
    )?;
    let authority =
        RuntimeModelAuthority::new(descriptor, account, credential, 1, timestamp()?, None)?;
    Ok(authority)
}

fn provider_configuration(
    descriptor: &ResolvedModelDescriptor,
) -> TestResult<CompiledProviderConfiguration> {
    let authority = runtime_authority(descriptor)?;
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(
            NonZeroU32::new(MAX_OUTPUT_TOKENS).ok_or("nonzero presentation output bound")?,
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("presentation-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("presentation-route-v1")?,
            ModelAdapterProfileId::new("presentation.auth")?,
            ProviderProfileRevisionId::new("presentation-auth-v1")?,
            ProviderAuthorityClassId::new("presentation-scripted")?,
            CredentialSelectionId::new("presentation-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable = ResolvedProviderAuthorityBinding::new(
        descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"presentation-semantics-v1".to_vec())?,
    )?;
    let family = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().ok_or("missing family")?,
        1,
        1,
        1,
        b"presentation-family-policy-v1".to_vec(),
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            descriptor,
            1,
        )?,
        stable,
        behavior,
        family,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

fn user_item() -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        id::<EntryId>(6)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(PROMPT, None)?)],
        Provenance::UserInput,
    )?)
}

fn assistant_item(entry: u128, text: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        id::<EntryId>(entry)?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new(
            text,
            Some(TextPhase::Final),
        )?)],
        Provenance::ModelOutput,
    )?)
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
    let item = assistant_item(entry, text)?;
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

fn workspace() -> TestResult<(TempDir, Arc<WorkspacePolicy>)> {
    let directory = tempfile::tempdir()?;
    let policy = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    Ok((directory, policy))
}

fn identity_sources() -> TestResult<(Arc<FixedClock>, Arc<SequenceIdGenerator>)> {
    Ok((
        Arc::new(FixedClock::new(timestamp()?)),
        Arc::new(SequenceIdGenerator::new(
            (1_u128..=32).map(Uuid::from_u128),
        )?),
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
