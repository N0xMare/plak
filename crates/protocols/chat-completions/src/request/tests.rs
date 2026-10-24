use std::collections::BTreeMap;
use std::num::NonZeroU32;

use plakous_model::CapabilityRequirement;
use plakous_model::{
    AttemptId, BranchId, Capability, CapabilityConstraints, CapabilitySet, ConfiguredTool,
    ConversationItem, ConversationView, DataClassification, EffectiveProviderConfigurationId,
    EntryId, FactProvenance, FactSource, FeatureSupport, InstructionBlock, ModelAdapterProfileId,
    ModelCacheAffinity, ModelFamilyId, ModelRequest, OpaqueBlock, Provenance, ReasoningEffort,
    ReasoningVisibility, ReplayEnvelope, ReplayKindId, ReplayScope, RequestMetadata,
    ResolvedModelDescriptor, RouteId, RunId, SessionId, Support, TextBlock, Timestamp,
    ToolCallerDisposition, ToolChoice, ToolConcurrency, ToolDefinition, ToolEffect,
    ToolExecutionDisposition, ToolOutputContract, ToolResultBlock, ToolResultUpdateBlock, TurnId,
    ValidationMode,
};
use uuid::Uuid;

use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
    descriptor: ResolvedModelDescriptor,
    tools: Vec<ConfiguredTool>,
}

fn configured_tool(name: &str) -> TestResult<ConfiguredTool> {
    Ok(ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new(name)?,
            "1",
            format!("{name} tool"),
            ToolInputContract::structured(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::utf8(1024)?,
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    ))
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"chat-request-selected"],
        )?)?)?;
        let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"chat-request-effective"],
        )?)?)?;
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(1_700_000_000, 0)?,
            None,
        );
        let features = [
            Capability::Streaming,
            Capability::Tools,
            Capability::Caching,
            Capability::Reasoning,
        ]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            )
        })
        .collect();
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new("openrouter.chat-completions")?,
            RouteId::new("openrouter-chat-test")?,
            ModelId::new("model/exact")?,
            None,
            None,
            ModelAdapterProfileId::new("chat-test")?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let tools = ["alpha", "beta"]
            .into_iter()
            .map(configured_tool)
            .collect::<TestResult<Vec<_>>>()?;
        Ok(Self {
            selected,
            effective,
            descriptor,
            tools,
        })
    }

    fn plan(&self, items: Vec<ConversationItem>) -> TestResult<ValidatedRequestPlan> {
        let view = if items.len() > 4_096 {
            let segments = items
                .chunks(2_048)
                .map(|chunk| {
                    plakous_model::HistorySegment::new(
                        plakous_model::HistorySegmentKind::ReasoningReplay,
                        chunk.to_vec(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            ConversationView::from_shared(
                plakous_model::CanonicalHistory::new(segments)?,
                plakous_model::WorkingProjection::default(),
            )
        } else {
            ConversationView::from_items(items)?
        };
        let request = ModelRequest::builder(
            self.selected,
            self.effective,
            view,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(self.tools.clone())
        .build()?;
        Ok(self
            .descriptor
            .validate_request(request, ValidationMode::Strict)?)
    }

    fn plan_with_instructions(
        &self,
        items: Vec<ConversationItem>,
        instructions: Vec<InstructionBlock>,
    ) -> TestResult<ValidatedRequestPlan> {
        let request = ModelRequest::builder(
            self.selected,
            self.effective,
            ConversationView::from_items(items)?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(self.tools.clone())
        .instructions(instructions)
        .build()?;
        Ok(self
            .descriptor
            .validate_request(request, ValidationMode::Strict)?)
    }

    fn policy(&self, plan: &ValidatedRequestPlan) -> TestResult<ChatRequestPolicy> {
        Ok(ChatRequestPolicy::new(
            ProtocolId::new("openrouter.chat-completions")?,
            ModelId::new("model/exact")?,
            plan.binding_digest(),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
            self.selected,
            self.effective,
            512,
            ChatOutputLimitField::MaxCompletionTokens,
            ChatReasoningReplay::VisibleOnly,
            None,
            ReasoningPolicy::Disabled,
            crate::stream::ReasoningAliasPolicy::Reasoning,
        )?)
    }

    fn exact_details_policy(&self, plan: &ValidatedRequestPlan) -> TestResult<ChatRequestPolicy> {
        Ok(ChatRequestPolicy::new(
            ProtocolId::new("openrouter.chat-completions")?,
            ModelId::new("model/exact")?,
            plan.binding_digest(),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
            self.selected,
            self.effective,
            512,
            ChatOutputLimitField::MaxCompletionTokens,
            ChatReasoningReplay::ExactDetails,
            None,
            ReasoningPolicy::Disabled,
            crate::stream::ReasoningAliasPolicy::Reasoning,
        )?)
    }
}

fn reasoning_detail_replay(
    policy: &ChatRequestPolicy,
    exact_json: &str,
    ordinal: u32,
) -> TestResult<ContentBlock> {
    let bound = ChatReasoningDetailReplayV1::new(
        policy.replay_target(),
        policy.reasoning(),
        policy.reasoning_alias(),
        policy.reasoning_alias(),
        exact_json.to_owned(),
        ordinal,
    )?;
    let payload = serde_json::value::to_raw_value(&bound)?;
    let envelope = ReplayEnvelope::new(
        policy.protocol().clone(),
        None,
        ReplayScope::ExactModel(ModelId::new("model/exact")?),
        ReplayKindId::new(REASONING_DETAIL_REPLAY_KIND)?,
        CHAT_REPLAY_SCHEMA_REVISION,
        DataClassification::Sensitive,
        payload,
    )?;
    Ok(ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
        ReasoningVisibility::OpaqueReplay,
        None,
        Some(envelope),
    )?))
}

fn detail_values(body: &Value, message_index: usize) -> Vec<Value> {
    body["messages"][message_index]["reasoning_details"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn seed_plan(fixture: &Fixture) -> TestResult<ValidatedRequestPlan> {
    fixture.plan(vec![item(
        100,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("seed", None)?)],
    )?])
}

fn item(id: u128, actor: Actor, blocks: Vec<ContentBlock>) -> TestResult<ConversationItem> {
    let provenance = match actor {
        Actor::User => Provenance::UserInput,
        Actor::Assistant => Provenance::ModelOutput,
        Actor::Tool => Provenance::ToolExecution,
        _ => Provenance::Instruction,
    };
    Ok(ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(id))?,
        actor,
        blocks,
        provenance,
    )?)
}

fn call(id: u128, name: &str, remote: &str) -> TestResult<(CallId, ToolCorrelation, ContentBlock)> {
    call_with_state(id, name, remote, ToolCallState::Requested)
}

fn call_with_state(
    id: u128,
    name: &str,
    remote: &str,
    state: ToolCallState,
) -> TestResult<(CallId, ToolCorrelation, ContentBlock)> {
    call_with_raw_state(id, name, remote, state, "{}")
}

fn call_with_raw_state(
    id: u128,
    name: &str,
    remote: &str,
    state: ToolCallState,
    raw_arguments: &str,
) -> TestResult<(CallId, ToolCorrelation, ContentBlock)> {
    let call_id = CallId::try_from(Uuid::from_u128(id))?;
    let correlation =
        ToolCorrelation::new(Some(remote.to_owned()), None, ToolCallerLineage::Direct)?;
    let block = ContentBlock::ToolCall(plakous_model::ToolCallBlock::new(
        call_id,
        ToolName::new(name)?,
        ToolExecutionOwner::Local,
        state,
        correlation.clone(),
        raw_arguments,
    )?);
    Ok((call_id, correlation, block))
}

fn result(call_id: CallId, correlation: ToolCorrelation, text: &str) -> TestResult<ContentBlock> {
    Ok(ContentBlock::ToolResult(ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TextBlock::new(text, None)?)],
        correlation,
    )?))
}

fn result_update(
    call_id: CallId,
    correlation: ToolCorrelation,
    text: &str,
) -> TestResult<ContentBlock> {
    Ok(ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        TextBlock::new(text, None)?,
        correlation,
    )?))
}

fn foreign_responses_opaque(fallback: OpaqueFallback) -> TestResult<ContentBlock> {
    let payload = serde_json::value::to_raw_value("encrypted-response-item")?;
    let replay = ReplayEnvelope::new(
        ProtocolId::new("openai.codex.responses")?,
        Some(ModelFamilyId::new("gpt")?),
        ReplayScope::ExactModel(ModelId::new("gpt-5.6-sol")?),
        ReplayKindId::new("responses.output_item")?,
        1,
        DataClassification::Sensitive,
        payload,
    )?;
    Ok(ContentBlock::Opaque(OpaqueBlock::new(replay, fallback)?))
}

#[test]
fn seeded_content_filter_guidance_lowers_on_chat() -> TestResult {
    let instruction =
        "<content_filter_guidance>\nOffer a permitted alternative.\n</content_filter_guidance>";
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![
        item(
            100,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "original request",
                None,
            )?)],
        )?,
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(101))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(instruction, None)?)],
            Provenance::Instruction,
        )?,
    ])?;
    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], instruction);
    Ok(())
}

#[test]
fn profile_and_harness_instructions_lower_as_system_developer_stays_developer() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan_with_instructions(
        vec![item(
            10,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
        )?],
        vec![
            InstructionBlock::new(InstructionSource::Profile, "profile text")?,
            InstructionBlock::new(InstructionSource::Harness, "harness text")?,
            InstructionBlock::new(InstructionSource::Developer, "developer text")?,
            InstructionBlock::new(InstructionSource::System, "system text")?,
        ],
    )?;
    let body: Value =
        serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
    assert_eq!(
        body["messages"][0],
        serde_json::json!({"role": "system", "content": "profile text"})
    );
    assert_eq!(
        body["messages"][1],
        serde_json::json!({"role": "system", "content": "harness text"})
    );
    assert_eq!(
        body["messages"][2],
        serde_json::json!({"role": "developer", "content": "developer text"})
    );
    assert_eq!(
        body["messages"][3],
        serde_json::json!({"role": "system", "content": "system text"})
    );
    assert_eq!(body["messages"][4]["role"], "user");
    Ok(())
}

#[test]
fn lowers_stateless_text_and_local_structured_tools_deterministically() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(11, "alpha", "remote-alpha")?;
    let plan = fixture.plan(vec![
        item(
            10,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
        )?,
        item(
            11,
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some("because".to_owned()),
                    None,
                )?),
                call,
            ],
        )?,
        item(12, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;
    let first = lower_request(&plan, &fixture.policy(&plan)?)?;
    let second = lower_request(&plan, &fixture.policy(&plan)?)?;
    assert_eq!(first.body(), second.body());
    assert_eq!(first.operation_digest(), second.operation_digest());
    let body: Value = serde_json::from_slice(first.body())?;
    assert_eq!(body["model"], "model/exact");
    assert_eq!(body["messages"][0]["content"], "hello");
    assert_eq!(body["messages"][1]["reasoning"], "because");
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "remote-alpha");
    assert_eq!(body["messages"][2]["tool_call_id"], "remote-alpha");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["max_completion_tokens"], 512);
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("artifact").is_none());
    assert!(body.get("previous_response_id").is_none());
    Ok(())
}

#[test]
fn skips_foreign_opaque_only_assistant_items() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![
        item(
            30,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("before", None)?)],
        )?,
        item(
            31,
            Actor::Assistant,
            vec![foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?],
        )?,
        item(
            32,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("after", None)?)],
        )?,
    ])?;

    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role": "user", "content": "before"},
            {"role": "user", "content": "after"}
        ])
    );
    Ok(())
}

#[test]
fn native_compaction_refuses_before_chat_body_is_built() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        41,
        Actor::Assistant,
        vec![ContentBlock::Compaction(
            plakous_model::CompactionBlock::new("opaque-ciphertext")?.with_native_item(
                r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#.into(),
            ),
        )],
    )?])?;
    let error = lower_request(&plan, &fixture.policy(&plan)?)
        .err()
        .ok_or("Chat must not send native ciphertext as text")?;
    assert_eq!(error.kind(), ChatRequestErrorKind::ProtectedHistory);
    assert!(!error.message().contains("opaque-ciphertext"));
    Ok(())
}

#[test]
fn compact_history_replays_compaction_markers_as_assistant_text() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![
        item(
            40,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("before", None)?)],
        )?,
        item(
            41,
            Actor::Assistant,
            vec![ContentBlock::Compaction(
                plakous_model::CompactionBlock::new("earlier turns")?,
            )],
        )?,
        item(
            42,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("summarize", None)?)],
        )?,
    ])?;
    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"], "earlier turns");
    assert_eq!(body["messages"][2]["content"], "summarize");
    Ok(())
}

#[test]
fn preserves_text_beside_omitted_foreign_opaque_blocks() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![
        item(
            33,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new("compatible", None)?),
                foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
            ],
        )?,
        item(
            34,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new("omitted", None)?),
                foreign_responses_opaque(OpaqueFallback::Omit)?,
            ],
        )?,
    ])?;

    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["messages"][0]["content"], "compatible");
    assert_eq!(body["messages"][1]["content"], "omitted");
    Ok(())
}

#[test]
fn replays_signed_reasoning_details_before_tool_results_byte_identically() -> TestResult {
    let fixture = Fixture::new()?;
    let policy = fixture.exact_details_policy(&seed_plan(&fixture)?)?;
    let first_detail = r#"{"type":"reasoning.text","text":"plan the lookup","signature":"sig-one","id":"r1","format":"anthropic-claude-v1","index":0}"#;
    let second_detail = r#"{"type":"reasoning.text","text":"follow up thought","signature":"sig-two","id":"r2","format":"anthropic-claude-v1","index":0}"#;
    let (call_id, correlation, call) = call(50, "alpha", "remote-signed-replay")?;
    let plan = fixture.plan(vec![
        item(
            50,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("run tool", None)?)],
        )?,
        item(
            51,
            Actor::Assistant,
            vec![reasoning_detail_replay(&policy, first_detail, 0)?, call],
        )?,
        item(52, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        item(
            53,
            Actor::Assistant,
            vec![reasoning_detail_replay(&policy, second_detail, 0)?],
        )?,
    ])?;
    let lowered = lower_request(&plan, &policy)?;
    let body: Value = serde_json::from_slice(lowered.body())?;

    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(
        detail_values(&body, 1),
        vec![serde_json::from_str::<Value>(first_detail)?]
    );
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["id"],
        "remote-signed-replay"
    );
    assert_eq!(body["messages"][2]["tool_call_id"], "remote-signed-replay");
    assert_eq!(body["messages"][3]["role"], "assistant");
    assert_eq!(
        detail_values(&body, 3),
        vec![serde_json::from_str::<Value>(second_detail)?]
    );
    assert!(body["messages"][3].get("tool_calls").is_none());
    Ok(())
}

#[test]
fn long_openrouter_history_has_no_aggregate_replay_wall() -> TestResult {
    let fixture = Fixture::new()?;
    let policy = fixture.exact_details_policy(&seed_plan(&fixture)?)?;
    for (count, bytes) in [(4_097_u128, 1), (64, 60_000)] {
        let exact =
            serde_json::json!({"type":"reasoning.encrypted", "data":"e".repeat(bytes)}).to_string();
        let replay = reasoning_detail_replay(&policy, &exact, 0)?;
        let plan = fixture.plan(
            (0..count)
                .map(|index| item(10_000 + index, Actor::Assistant, vec![replay.clone()]))
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        let lowered = lower_request(&plan, &policy)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        let messages = body["messages"].as_array().ok_or("missing messages")?;
        assert_eq!(messages.len(), usize::try_from(count)?);
        for message in messages {
            assert_eq!(message["reasoning_details"][0]["data"], "e".repeat(bytes));
        }
    }
    Ok(())
}

#[test]
fn accepts_assistant_turn_with_empty_reasoning_details() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(60, "alpha", "remote-empty-details")?;
    let plan = fixture.plan(vec![
        item(60, Actor::Assistant, vec![call])?,
        item(61, Actor::Tool, vec![result(call_id, correlation, "ok")?])?,
    ])?;
    let policy = fixture.exact_details_policy(&plan)?;
    let lowered = lower_request(&plan, &policy)?;
    let body: Value = serde_json::from_slice(lowered.body())?;

    assert_eq!(body["messages"][0]["role"], "assistant");
    assert!(body["messages"][0].get("reasoning_details").is_none());
    assert_eq!(
        body["messages"][0]["tool_calls"][0]["id"],
        "remote-empty-details"
    );
    Ok(())
}

#[test]
fn drops_unsigned_reasoning_text_under_exact_details_while_preserving_encrypted() -> TestResult {
    let fixture = Fixture::new()?;
    let policy = fixture.exact_details_policy(&seed_plan(&fixture)?)?;
    let encrypted = r#"{"type":"reasoning.encrypted","data":"opaque-payload","id":"e1","format":"some-upstream","index":0}"#;
    let unsigned_visible = "unsigned visible reasoning";
    let (call_id, correlation, call_block) = call(70, "alpha", "remote-mixed-reasoning")?;

    let exact_plan = fixture.plan(vec![
        item(
            70,
            Actor::Assistant,
            vec![reasoning_detail_replay(&policy, encrypted, 0)?, call_block],
        )?,
        item(
            71,
            Actor::Tool,
            vec![result(call_id, correlation.clone(), "ok")?],
        )?,
    ])?;
    let exact_body: Value = serde_json::from_slice(lower_request(&exact_plan, &policy)?.body())?;
    assert_eq!(
        detail_values(&exact_body, 0),
        vec![serde_json::from_str::<Value>(encrypted)?]
    );
    assert!(exact_body["messages"][0].get("reasoning").is_none());

    let (visible_call_id, visible_correlation, visible_call_block) =
        call(72, "alpha", "remote-visible-only")?;
    let visible_plan = fixture.plan(vec![
        item(
            72,
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some(unsigned_visible.to_owned()),
                    None,
                )?),
                visible_call_block,
            ],
        )?,
        item(
            73,
            Actor::Tool,
            vec![result(visible_call_id, visible_correlation, "ok")?],
        )?,
    ])?;
    let visible_policy = fixture.policy(&visible_plan)?;
    let visible_body: Value =
        serde_json::from_slice(lower_request(&visible_plan, &visible_policy)?.body())?;
    assert_eq!(visible_body["messages"][0]["reasoning"], unsigned_visible);
    assert!(
        visible_body["messages"][0]
            .get("reasoning_details")
            .is_none()
    );
    Ok(())
}

#[test]
fn preserves_mixed_reasoning_details_order_and_encrypted_data_exactly() -> TestResult {
    let fixture = Fixture::new()?;
    let policy = fixture.exact_details_policy(&seed_plan(&fixture)?)?;
    let signed_text = r#"{"type":"reasoning.text","text":"step-a","signature":"sig-a","id":"r1","format":"anthropic-claude-v1","index":0}"#;
    let encrypted = r#"{"type":"reasoning.encrypted","data":"A\\u003dB","id":"r2","format":"anthropic-claude-v1","index":1}"#;
    let summary = r#"{"type":"reasoning.summary","text":"condensed","id":"r3","index":2}"#;
    let (call_id, correlation, call) = call(80, "alpha", "remote-mixed-details")?;
    let plan = fixture.plan(vec![
        item(
            80,
            Actor::Assistant,
            vec![
                reasoning_detail_replay(&policy, signed_text, 0)?,
                reasoning_detail_replay(&policy, encrypted, 1)?,
                reasoning_detail_replay(&policy, summary, 2)?,
                call,
            ],
        )?,
        item(81, Actor::Tool, vec![result(call_id, correlation, "ok")?])?,
    ])?;
    let lowered = lower_request(&plan, &policy)?;
    let body: Value = serde_json::from_slice(lowered.body())?;

    let expected = [
        serde_json::from_str::<Value>(signed_text)?,
        serde_json::from_str::<Value>(encrypted)?,
        serde_json::from_str::<Value>(summary)?,
    ];
    assert_eq!(detail_values(&body, 0), expected);
    assert_eq!(
        detail_values(&body, 0)[1]["data"],
        serde_json::from_str::<Value>(encrypted)?["data"]
    );
    Ok(())
}

#[test]
fn projects_foreign_opaque_semantic_summary_as_assistant_text() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        35,
        Actor::Assistant,
        vec![foreign_responses_opaque(OpaqueFallback::SemanticSummary(
            "portable summary".to_owned(),
        ))?],
    )?])?;

    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["messages"][0]["role"], "assistant");
    assert_eq!(body["messages"][0]["content"], "portable summary");
    Ok(())
}

#[test]
fn lowers_responses_tool_exchange_with_foreign_opaque_sibling() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(36, "alpha", "responses-call-alpha")?;
    let plan = fixture.plan(vec![
        item(
            36,
            Actor::Assistant,
            vec![
                call,
                foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
            ],
        )?,
        item(
            37,
            Actor::Tool,
            vec![
                result_update(call_id, correlation.clone(), "working\n")?,
                result(call_id, correlation, "done")?,
            ],
        )?,
    ])?;

    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(
        body["messages"][0]["tool_calls"][0]["id"],
        "responses-call-alpha"
    );
    assert_eq!(
        body["messages"][0]["tool_calls"][0]["function"]["name"],
        "alpha"
    );
    assert_eq!(body["messages"][1]["tool_call_id"], "responses-call-alpha");
    assert_eq!(body["messages"][1]["content"], "working\ndone");
    Ok(())
}

#[test]
fn a_run_of_tool_call_items_folds_into_one_assistant_message() -> TestResult {
    let fixture = Fixture::new()?;
    let (alpha_id, alpha_correlation, alpha) = call(30, "alpha", "remote-alpha")?;
    let (beta_id, beta_correlation, beta) = call(31, "beta", "remote-beta")?;
    let plan = fixture.plan(vec![
        item(30, Actor::Assistant, vec![alpha])?,
        item(31, Actor::Assistant, vec![beta])?,
        item(
            32,
            Actor::Tool,
            vec![
                result(alpha_id, alpha_correlation, "a")?,
                result(beta_id, beta_correlation, "b")?,
            ],
        )?,
    ])?;
    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    let messages = body["messages"].as_array().ok_or("messages")?;
    assert_eq!(messages.len(), 3, "{body}");
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "remote-alpha");
    assert_eq!(messages[0]["tool_calls"][1]["id"], "remote-beta");
    assert_eq!(messages[1]["tool_call_id"], "remote-alpha");
    assert_eq!(messages[2]["tool_call_id"], "remote-beta");
    Ok(())
}

#[test]
fn text_inside_an_open_tool_batch_is_refused() -> TestResult {
    let fixture = Fixture::new()?;
    let (alpha_id, alpha_correlation, alpha) = call(33, "alpha", "remote-alpha")?;
    let (beta_id, beta_correlation, beta) = call(34, "beta", "remote-beta")?;
    let plan = fixture.plan(vec![
        item(33, Actor::Assistant, vec![alpha])?,
        item(
            34,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("more", None)?), beta],
        )?,
        item(
            35,
            Actor::Tool,
            vec![
                result(alpha_id, alpha_correlation, "a")?,
                result(beta_id, beta_correlation, "b")?,
            ],
        )?,
    ])?;
    let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
        return Err("text folded behind an open tool call".into());
    };
    assert!(
        error.message().contains("intervened"),
        "{}",
        error.message()
    );
    Ok(())
}

#[test]
fn folds_canonical_tool_result_updates_into_the_terminal_chat_message() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(13, "alpha", "remote-alpha")?;
    let plan = fixture.plan(vec![
        item(13, Actor::Assistant, vec![call])?,
        item(
            14,
            Actor::Tool,
            vec![
                result_update(call_id, correlation.clone(), "working\n")?,
                result(call_id, correlation, "done")?,
            ],
        )?,
    ])?;

    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(body["messages"][1]["tool_call_id"], "remote-alpha");
    assert_eq!(body["messages"][1]["content"], "working\ndone");
    Ok(())
}

#[test]
fn mcp_owned_tool_exchange_lowers_exactly_like_a_local_one() -> TestResult {
    let fixture = Fixture::new()?;
    let lower = |owner: ToolExecutionOwner| -> TestResult<Value> {
        let (call_id, correlation, call) = call(13, "alpha", "remote-alpha")?;
        let ContentBlock::ToolCall(call) = call else {
            return Err("fixture call is a tool call".into());
        };
        let call = plakous_model::ToolCallBlock::new(
            call_id,
            call.exposed_name().clone(),
            owner.clone(),
            call.state(),
            correlation.clone(),
            call.raw_arguments(),
        )?;
        let plan = fixture.plan(vec![
            item(13, Actor::Assistant, vec![ContentBlock::ToolCall(call)])?,
            item(
                14,
                Actor::Tool,
                vec![
                    ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
                        call_id,
                        owner.clone(),
                        TextBlock::new("working\n", None)?,
                        correlation.clone(),
                    )?),
                    ContentBlock::ToolResult(ToolResultBlock::new(
                        call_id,
                        owner,
                        ToolResultKind::Success,
                        vec![ToolResultPart::Text(TextBlock::new("done", None)?)],
                        correlation,
                    )?),
                ],
            )?,
        ])?;
        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        Ok(serde_json::from_slice(lowered.body())?)
    };
    let mcp = ToolExecutionOwner::Mcp {
        server: plakous_model::McpServerId::new("github")?,
    };
    assert_eq!(lower(mcp)?, lower(ToolExecutionOwner::Local)?);
    Ok(())
}

#[test]
fn replays_provider_visible_tool_names_from_projected_history() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(15, "exec_command", "remote-exec")?;
    let plan = fixture.plan(vec![
        item(15, Actor::Assistant, vec![call])?,
        item(16, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;
    let tools = LoweredTools {
        wire: Vec::new(),
        exposed_names: BTreeMap::from([
            (ToolName::new("alpha")?, "exec_command".to_owned()),
            (ToolName::new("beta")?, "read_file".to_owned()),
        ]),
        exposures: BTreeSet::from(["exec_command".to_owned(), "read_file".to_owned()]),
    };

    let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
    assert_eq!(messages[0].tool_calls[0].function.name, "exec_command");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("remote-exec"));
    Ok(())
}

/// Two exposures over one canonical tool (pi's `edit` and `write` over the
/// patch tool) leave the canonical name unmapped, and each call recorded
/// under its exposed name still replays as that function.
#[test]
fn replays_each_exposure_of_a_canonical_tool_presented_twice() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(15, "write", "remote-write")?;
    let plan = fixture.plan(vec![
        item(15, Actor::Assistant, vec![call])?,
        item(16, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;
    let tools = LoweredTools {
        wire: Vec::new(),
        exposed_names: BTreeMap::new(),
        exposures: BTreeSet::from(["edit".to_owned(), "write".to_owned()]),
    };

    let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
    assert_eq!(messages[0].tool_calls[0].function.name, "write");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("remote-write"));
    Ok(())
}

/// A call records the canonical name it resolved to and the exact name
/// the model called; replay sends the exposed name, never the canonical
/// map's exposure of the canonical name.
#[test]
fn replays_the_recorded_exposed_name_not_the_canonical_mapping() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(15, "write", "remote-write")?;
    let ContentBlock::ToolCall(call) = call else {
        return Err("fixture call is not a tool call".into());
    };
    let call = ContentBlock::ToolCall(call.with_canonical_name(ToolName::new("apply_patch")?));
    let plan = fixture.plan(vec![
        item(15, Actor::Assistant, vec![call])?,
        item(16, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;
    let tools = LoweredTools {
        wire: Vec::new(),
        exposed_names: BTreeMap::from([(ToolName::new("apply_patch")?, "edit".to_owned())]),
        exposures: BTreeSet::from(["edit".to_owned(), "write".to_owned()]),
    };

    let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
    assert_eq!(messages[0].tool_calls[0].function.name, "write");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("remote-write"));
    Ok(())
}

fn png_image() -> TestResult<plakous_model::ImageBlock> {
    Ok(plakous_model::ImageBlock::new(
        "image/png",
        plakous_model::MediaSource::bytes(b"\x89PNG".to_vec())?,
    )?)
}

#[test]
fn user_images_lower_to_ordered_image_url_parts() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        15,
        Actor::User,
        vec![
            ContentBlock::Text(TextBlock::new("what is this", None)?),
            ContentBlock::Image(png_image()?),
        ],
    )?])?;
    let tools = LoweredTools {
        wire: Vec::new(),
        exposed_names: BTreeMap::new(),
        exposures: BTreeSet::new(),
    };
    let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
    assert_eq!(
        serde_json::to_value(&messages[0])?,
        serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "what is this"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw=="}}
        ]})
    );
    Ok(())
}

#[test]
fn tool_result_images_follow_the_round_as_one_synthetic_user_message() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call(15, "inspect_image", "remote-inspect")?;
    let result = ContentBlock::ToolResult(ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![
            ToolResultPart::Text(TextBlock::new("media_type=image/png", None)?),
            ToolResultPart::Image(png_image()?),
        ],
        correlation,
    )?);
    let plan = fixture.plan(vec![
        item(15, Actor::Assistant, vec![call])?,
        item(16, Actor::Tool, vec![result])?,
    ])?;
    let tools = LoweredTools {
        wire: Vec::new(),
        exposed_names: BTreeMap::new(),
        exposures: BTreeSet::from(["inspect_image".to_owned()]),
    };
    let lowered = |plan| -> TestResult<Value> {
        Ok(serde_json::to_value(lower_history(
            plan,
            &fixture.policy(plan)?,
            &tools,
        )?)?)
    };
    let messages = lowered(&plan)?;
    assert_eq!(
        messages[1],
        serde_json::json!({"role": "tool", "content": "media_type=image/png", "tool_call_id": "remote-inspect"})
    );
    assert_eq!(
        messages[2],
        serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "image from tool call remote-inspect:"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw=="}}
        ]})
    );
    assert_eq!(messages.as_array().map(Vec::len), Some(3));
    assert_eq!(lowered(&plan)?, messages);
    Ok(())
}

#[test]
fn projects_unexposed_historical_exchange_as_inert_assistant_text() -> TestResult {
    let fixture = Fixture::new()?;
    let historical_name = "retired-tool_012345678901234567890123456789012345678901234567890";
    assert_eq!(historical_name.len(), 64);
    let (call_id, correlation, call) = call_with_state(
        17,
        historical_name,
        "remote-retired",
        ToolCallState::Completed,
    )?;
    let plan = fixture.plan(vec![
        item(17, Actor::Assistant, vec![call])?,
        item(18, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;

    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["messages"][0]["role"], "assistant");
    assert!(body["messages"][0].get("tool_calls").is_none());
    assert!(body["messages"][0]["content"].as_str().is_some_and(|text| {
        text.contains(historical_name) && text.contains("\"kind\":\"call\"")
    }));
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert!(body["messages"][1].get("tool_call_id").is_none());
    assert!(
        body["messages"][1]["content"]
            .as_str()
            .is_some_and(|text| text.contains("\"kind\":\"result\"") && text.contains("done"))
    );
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["tools"][0]["function"]["name"], "alpha");
    assert_eq!(body["tools"][1]["function"]["name"], "beta");
    Ok(())
}

#[test]
fn preserves_exact_raw_completed_freeform_in_inert_history() -> TestResult {
    let fixture = Fixture::new()?;
    let raw = "const value = `quoted \\\"line\\\"`;\ntext(\"λ\\\\tail\");";
    let (call_id, correlation, call) = call_with_raw_state(
        19,
        "retired_exec",
        "remote-freeform",
        ToolCallState::Completed,
        raw,
    )?;
    let plan = fixture.plan(vec![
        item(19, Actor::Assistant, vec![call])?,
        item(20, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;

    let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    let content = body["messages"][0]["content"]
        .as_str()
        .ok_or("historical record is absent")?;
    let record: Value = serde_json::from_str(
        content
            .strip_prefix("[Historical tool record; unavailable in the current target] ")
            .ok_or("historical record prefix is absent")?,
    )?;
    assert_eq!(record["value"], raw);
    assert_eq!(record["name"], "retired_exec");
    assert!(body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .all(|tool| tool["function"]["name"] != "retired_exec")
    }));
    Ok(())
}

#[test]
fn preserves_responses_shaped_requested_freeform_as_inert_history() -> TestResult {
    let fixture = Fixture::new()?;
    let raw = r#""const value = await tools.exec_command({cmd: 'pwd'});\ntext(value);""#;
    let (call_id, correlation, call) = call_with_raw_state(
        21,
        "exec",
        "remote-requested-freeform",
        ToolCallState::Requested,
        raw,
    )?;
    let complete = fixture.plan(vec![
        item(21, Actor::Assistant, vec![call])?,
        item(22, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;
    let lowered = lower_request(&complete, &fixture.policy(&complete)?)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    let content = body["messages"][0]["content"]
        .as_str()
        .ok_or("historical record is absent")?;
    let record: Value = serde_json::from_str(
        content
            .strip_prefix("[Historical tool record; unavailable in the current target] ")
            .ok_or("historical record prefix is absent")?,
    )?;
    assert_eq!(record["value"], raw);
    assert_eq!(record["name"], "exec");
    Ok(())
}

#[test]
fn inert_history_is_bounded_and_never_rewrites_current_exposure() -> TestResult {
    let fixture = Fixture::new()?;
    let (call_id, correlation, call) = call_with_raw_state(
        40,
        "alpha",
        "remote-bound",
        ToolCallState::Completed,
        "freeform",
    )?;
    let plan = fixture.plan(vec![
        item(40, Actor::Assistant, vec![call])?,
        item(41, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
    ])?;
    let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
        return Err("current free-form tool history was silently rewritten".into());
    };
    assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
    assert_eq!(
        error.message(),
        "Chat structured tool arguments must be duplicate-free JSON objects"
    );

    let mut record = String::new();
    append_inert_history(
        &mut record,
        "call",
        "retired",
        &"x".repeat(MAX_INERT_HISTORICAL_TOOL_BYTES),
    )?;
    assert!(record.contains("Historical tool record"));
    let Err(error) = append_inert_history(
        &mut String::new(),
        "call",
        "retired",
        &"x".repeat(MAX_INERT_HISTORICAL_TOOL_BYTES + 1),
    ) else {
        return Err("oversized inert historical record was accepted".into());
    };
    assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
    Ok(())
}

#[test]
fn unexposed_historical_names_never_become_chat_functions() -> TestResult {
    let fixture = Fixture::new()?;
    for (ordinal, historical_name) in [
        "retired.tool".to_owned(),
        "retired/tool".to_owned(),
        "retired:tool".to_owned(),
        "a".repeat(65),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 20 + ordinal as u128 * 2;
        let (call_id, correlation, call) = call_with_state(
            id,
            &historical_name,
            &format!("remote-invalid-{ordinal}"),
            ToolCallState::Completed,
        )?;
        let plan = fixture.plan(vec![
            item(id, Actor::Assistant, vec![call])?,
            item(
                id + 1,
                Actor::Tool,
                vec![result(call_id, correlation, "done")?],
            )?,
        ])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert!(body["messages"][0].get("tool_calls").is_none());
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .is_some_and(|content| content.contains(&historical_name))
        );
    }
    Ok(())
}

#[test]
fn chat_function_name_grammar_is_exact() {
    assert!(is_chat_function_name("a"));
    assert!(is_chat_function_name("Az09_-"));
    assert!(is_chat_function_name(&"a".repeat(64)));
    assert!(!is_chat_function_name(""));
    assert!(!is_chat_function_name(&"a".repeat(65)));
    assert!(!is_chat_function_name("retired.tool"));
    assert!(!is_chat_function_name("tool/name"));
    assert!(!is_chat_function_name("tool:name"));
    assert!(!is_chat_function_name("tool name"));
    assert!(!is_chat_function_name("tööl"));
}

#[test]
fn rejects_current_tool_names_outside_chat_function_grammar() -> TestResult {
    for invalid in [
        "tool.name".to_owned(),
        "tool/name".to_owned(),
        "a".repeat(65),
    ] {
        let mut fixture = Fixture::new()?;
        fixture.tools = vec![configured_tool(&invalid)?];
        let plan = fixture.plan(vec![item(
            14,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("inspect", None)?)],
        )?])?;
        let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
            return Err("invalid current Chat function name was accepted".into());
        };
        assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
        assert_eq!(
            error.message(),
            "configured tool name is outside Chat function-name grammar"
        );
    }
    Ok(())
}

#[test]
fn strict_gateway_managed_and_omitted_router_shapes_are_exact() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        15,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("route", None)?)],
    )?])?;
    let managed = lower_request(&plan, &fixture.policy(&plan)?)?;
    let managed_body: Value = serde_json::from_slice(managed.body())?;
    assert!(managed_body.get("provider").is_none());

    let mut legacy_limit_policy = fixture.policy(&plan)?;
    legacy_limit_policy.output_limit_field = ChatOutputLimitField::MaxTokens;
    let legacy_limit = lower_request(&plan, &legacy_limit_policy)?;
    let legacy_limit_body: Value = serde_json::from_slice(legacy_limit.body())?;
    assert_eq!(legacy_limit_body["max_tokens"], 512);
    assert!(legacy_limit_body.get("max_completion_tokens").is_none());
    assert_ne!(managed.operation_digest(), legacy_limit.operation_digest());

    let mut gateway_policy = fixture.policy(&plan)?;
    gateway_policy.router = Some(ChatRouterPolicy::gateway_managed(
        Some("deny".to_owned()),
        Some(true),
    )?);
    let gateway = lower_request(&plan, &gateway_policy)?;
    let gateway_body: Value = serde_json::from_slice(gateway.body())?;
    assert_eq!(
        gateway_body["provider"],
        serde_json::json!({
            "allow_fallbacks": false,
            "require_parameters": true,
            "data_collection": "deny",
            "zdr": true
        })
    );
    assert!(gateway_body["provider"].get("only").is_none());
    assert!(gateway_body["provider"].get("order").is_none());

    let mut strict_policy = fixture.policy(&plan)?;
    strict_policy.router = Some(ChatRouterPolicy::strict("endpoint/exact", "deny", true)?);
    let strict = lower_request(&plan, &strict_policy)?;
    let strict_body: Value = serde_json::from_slice(strict.body())?;
    assert_eq!(
        strict_body["provider"],
        serde_json::json!({
            "only": ["endpoint/exact"],
            "order": ["endpoint/exact"],
            "allow_fallbacks": false,
            "require_parameters": true,
            "data_collection": "deny",
            "zdr": true
        })
    );
    assert_ne!(managed.operation_digest(), strict.operation_digest());
    assert_ne!(managed.operation_digest(), gateway.operation_digest());
    assert!(ChatRouterPolicy::strict("router", "deny", true).is_err());
    assert!(ChatRouterPolicy::strict("endpoint/exact", "DENY", true).is_err());
    assert!(ChatRouterPolicy::gateway_managed(Some("DENY".to_owned()), Some(true)).is_err());
    Ok(())
}

#[test]
fn openrouter_context_compression_plugin_is_disabled() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        16,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
    )?])?;
    let policy = fixture
        .policy(&plan)?
        .with_plugins(OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS);
    let lowered = lower_request(&plan, &policy)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(
        body["plugins"],
        serde_json::json!([{ "id": "context-compression", "enabled": false }])
    );
    Ok(())
}

#[test]
fn gateway_managed_requires_parameters_without_tools_or_reasoning() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.tools.clear();
    let plan = fixture.plan(vec![item(
        16,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("route", None)?)],
    )?])?;
    let mut policy = fixture.policy(&plan)?;
    policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);

    let lowered = lower_request(&plan, &policy)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(
        body["provider"],
        serde_json::json!({
            "allow_fallbacks": false,
            "require_parameters": true
        })
    );
    assert!(body.get("tools").is_none());
    assert!(body.get("reasoning").is_none());
    Ok(())
}

#[test]
fn reasoning_is_exact_explicit_and_fails_closed() -> TestResult {
    use plakous_model::CapabilityRequirement;

    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        16,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("reason", None)?)],
    )?])?;
    let mut mismatched = fixture.policy(&plan)?;
    mismatched.reasoning = ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::High),
        requirement: CapabilityRequirement::Required,
    };
    let Err(error) = lower_request(&plan, &mismatched) else {
        return Err("reasoning mismatch was accepted".into());
    };
    assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
    assert!(error.message().contains("does not match"));

    let efforts = [
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::XHigh, "xhigh"),
        (ReasoningEffort::Max, "max"),
        (ReasoningEffort::Ultra, "ultra"),
    ];
    for (effort, spelling) in efforts {
        let wire = lower_reasoning(
            ReasoningPolicy::Enabled {
                effort: Some(effort),
                requirement: CapabilityRequirement::Required,
            },
            ChatDisabledReasoningWire::Omit,
        )?
        .ok_or("enabled reasoning was omitted")?;
        let value = serde_json::to_value(wire)?;
        assert_eq!(
            value,
            serde_json::json!({"effort": spelling, "exclude": false})
        );
    }
    let budget =
        ReasoningEffort::TokenBudget(NonZeroU32::new(128).ok_or("nonzero reasoning budget")?);
    let Err(error) = lower_reasoning(
        ReasoningPolicy::Enabled {
            effort: Some(budget),
            requirement: CapabilityRequirement::Required,
        },
        ChatDisabledReasoningWire::Omit,
    ) else {
        return Err("token-budget reasoning was accepted".into());
    };
    assert_eq!(error.kind(), ChatRequestErrorKind::UnsupportedSemantic);
    Ok(())
}

#[test]
fn rejects_out_of_order_tool_results_even_when_all_ids_exist() -> TestResult {
    let fixture = Fixture::new()?;
    let (alpha_id, alpha_correlation, alpha) = call(21, "alpha", "remote-alpha")?;
    let (beta_id, beta_correlation, beta) = call(22, "beta", "remote-beta")?;
    let plan = fixture.plan(vec![
        item(20, Actor::Assistant, vec![alpha, beta])?,
        item(
            21,
            Actor::Tool,
            vec![result(beta_id, beta_correlation, "beta")?],
        )?,
        item(
            22,
            Actor::Tool,
            vec![result(alpha_id, alpha_correlation, "alpha")?],
        )?,
    ])?;
    let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
        return Err("swapped results were accepted".into());
    };
    assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
    assert!(error.message().contains("order or correlation"));
    Ok(())
}

#[test]
fn gateway_session_key_emits_session_id_without_prompt_cache_key_or_cache_control() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        40,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("sticky", None)?)],
    )?])?;
    let mut policy = fixture
        .policy(&plan)?
        .with_gateway_session_key("affinity-hex");
    policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);
    let lowered = lower_request(&plan, &policy)?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["session_id"], "affinity-hex");
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("cache_control").is_none());
    assert_eq!(body["provider"]["require_parameters"], true);
    Ok(())
}

#[test]
fn forked_gateway_session_key_does_not_reuse_source_session_id() -> TestResult {
    let session = SessionId::try_from(Uuid::from_u128(91_001))?;
    let source = ModelCacheAffinity::from_runtime_entropy(
        session,
        BranchId::try_from(Uuid::from_u128(91_002))?,
        0,
        [0x41; 32],
    )?;
    let fork = ModelCacheAffinity::from_runtime_entropy(
        session,
        BranchId::try_from(Uuid::from_u128(91_003))?,
        0,
        [0x42; 32],
    )?;
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        43,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("fork", None)?)],
    )?])?;
    let mut source_policy = fixture
        .policy(&plan)?
        .with_gateway_session_key(source.identity().to_string());
    source_policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);
    let mut fork_policy = fixture
        .policy(&plan)?
        .with_gateway_session_key(fork.identity().to_string());
    fork_policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);
    let source_body: Value = serde_json::from_slice(lower_request(&plan, &source_policy)?.body())?;
    let fork_body: Value = serde_json::from_slice(lower_request(&plan, &fork_policy)?.body())?;
    assert_eq!(source_body["session_id"], source.identity().to_string());
    assert_eq!(fork_body["session_id"], fork.identity().to_string());
    assert_ne!(source_body["session_id"], fork_body["session_id"]);
    assert!(source_body.get("prompt_cache_key").is_none());
    assert!(fork_body.get("prompt_cache_key").is_none());
    assert!(source_body.get("cache_control").is_none());
    assert!(fork_body.get("cache_control").is_none());
    Ok(())
}

#[test]
fn chat_function_strict_requires_every_property() {
    assert!(chat_function_strict(&serde_json::json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"]
    })));
    assert!(!chat_function_strict(&serde_json::json!({
        "type": "object",
        "properties": {
            "patch": {"type": "string"},
            "file_path": {"type": "string"}
        },
        "required": []
    })));
}

#[test]
fn union_function_schema_lowers_strict_false_complete_schema_strict_true() -> TestResult {
    fn tool_with_schema(name: &str, schema: Value) -> TestResult<ConfiguredTool> {
        Ok(ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new(name)?,
                "1",
                format!("{name} tool"),
                ToolInputContract::structured(schema)?,
                ToolOutputContract::utf8(1024)?,
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        ))
    }
    let mut fixture = Fixture::new()?;
    fixture.tools = vec![
        tool_with_schema(
            "apply_patch",
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "patch": {"type": "string"},
                    "file_path": {"type": "string"}
                },
                "required": [],
                "additionalProperties": false
            }),
        )?,
        tool_with_schema(
            "read_file",
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )?,
    ];
    let plan = fixture.plan(vec![item(
        42,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("patch", None)?)],
    )?])?;
    let body: Value =
        serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
    assert_eq!(body["tools"][0]["function"]["name"], "apply_patch");
    assert_eq!(body["tools"][0]["function"]["strict"], false);
    assert_eq!(body["tools"][1]["function"]["name"], "read_file");
    assert_eq!(body["tools"][1]["function"]["strict"], true);
    Ok(())
}

#[test]
fn tool_choice_none_legacy_shape_when_omit_knob_off() -> TestResult {
    let fixture = Fixture::new()?;
    let request = ModelRequest::builder(
        fixture.selected,
        fixture.effective,
        ConversationView::from_items(vec![item(
            41,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("compact", None)?)],
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .tools(fixture.tools.clone())
    .tool_choice(ToolChoice::None)
    .build()?;
    let plan = fixture
        .descriptor
        .validate_request(request, ValidationMode::Strict)?;
    let body: Value =
        serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
    assert_eq!(body["tool_choice"], "none");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    Ok(())
}

#[test]
fn tool_choice_none_omits_tools_when_omit_knob_on() -> TestResult {
    let fixture = Fixture::new()?;
    let request = ModelRequest::builder(
        fixture.selected,
        fixture.effective,
        ConversationView::from_items(vec![item(
            41,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("compact", None)?)],
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .tools(fixture.tools.clone())
    .tool_choice(ToolChoice::None)
    .build()?;
    let plan = fixture
        .descriptor
        .validate_request(request, ValidationMode::Strict)?;
    let policy = fixture
        .policy(&plan)?
        .with_tool_choice_none_omits_tools(true);
    let body: Value = serde_json::from_slice(lower_request(&plan, &policy)?.body())?;
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    Ok(())
}

/// A harness summary request: the exchange so far, then the instruction as
/// the trailing user message, under `ToolChoice::None` with the turn's tools.
fn summary_request_plan(fixture: &Fixture) -> TestResult<ValidatedRequestPlan> {
    let bound = plakous_model::semantic_compaction_output_bound(
        NonZeroU32::new(512),
        ReasoningPolicy::Disabled,
    );
    let request = ModelRequest::builder(
        fixture.selected,
        fixture.effective,
        ConversationView::from_items(vec![
            item(
                41,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("fix the parser", None)?)],
            )?,
            item(
                42,
                Actor::Assistant,
                vec![ContentBlock::Text(TextBlock::new("fixed", None)?)],
            )?,
            item(
                43,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new(
                    plakous_model::SEMANTIC_COMPACTION_INSTRUCTION,
                    None,
                )?)],
            )?,
        ])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .tools(fixture.tools.clone())
    .tool_choice(ToolChoice::None)
    .limits(plakous_model::GenerationLimits::new(
        Some(bound),
        Vec::new(),
    )?)
    .build()?;
    Ok(fixture
        .descriptor
        .validate_request(request, ValidationMode::Strict)?)
}

/// The summary request ends on the exact instruction as a user message under
/// either tool-omission setting; the setting decides only whether the tools
/// travel with `tool_choice: "none"` or both are omitted.
#[test]
fn summary_request_ends_on_the_instruction_under_either_tool_policy() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = summary_request_plan(&fixture)?;
    for omit in [false, true] {
        let mut policy = fixture
            .policy(&plan)?
            .with_tool_choice_none_omits_tools(omit)
            .with_plugins(OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS);
        policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);
        let body: Value = serde_json::from_slice(lower_request(&plan, &policy)?.body())?;
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .ok_or("summary request has no messages")?;
        assert_eq!(last["role"], "user");
        assert_eq!(body["max_completion_tokens"], 512);
        assert_eq!(body["provider"]["require_parameters"], true);
        assert!(body.get("reasoning").is_none());
        assert_eq!(
            body["plugins"],
            serde_json::json!([{"id": "context-compression", "enabled": false}])
        );
        assert_eq!(
            last["content"],
            plakous_model::SEMANTIC_COMPACTION_INSTRUCTION
        );
        if omit {
            assert!(body.get("tools").is_none());
            assert!(body.get("tool_choice").is_none());
        } else {
            assert_eq!(body["tool_choice"], "none");
            assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        }
    }
    Ok(())
}

#[test]
fn tool_choice_auto_omission_is_dialect_controlled() -> TestResult {
    let fixture = Fixture::new()?;
    let plan = fixture.plan(vec![item(
        41,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("code", None)?)],
    )?])?;
    let body: Value =
        serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    let policy = fixture
        .policy(&plan)?
        .with_tool_choice_auto_uses_default(true);
    let body: Value = serde_json::from_slice(lower_request(&plan, &policy)?.body())?;
    assert!(body.get("tool_choice").is_none());
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    Ok(())
}

#[test]
fn automatic_default_policy_preserves_explicit_choices() -> TestResult {
    let fixture = Fixture::new()?;
    for (choice, expected) in [
        (ToolChoice::None, serde_json::json!("none")),
        (ToolChoice::Required, serde_json::json!("required")),
        (
            ToolChoice::Specific(ToolName::new("alpha")?),
            serde_json::json!({"type":"function","function":{"name":"alpha"}}),
        ),
    ] {
        let request = ModelRequest::builder(
            fixture.selected,
            fixture.effective,
            ConversationView::from_items(vec![item(
                41,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("code", None)?)],
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(fixture.tools.clone())
        .tool_choice(choice)
        .build()?;
        let plan = fixture
            .descriptor
            .validate_request(request, ValidationMode::Strict)?;
        let policy = fixture
            .policy(&plan)?
            .with_tool_choice_auto_uses_default(true);
        let body: Value = serde_json::from_slice(lower_request(&plan, &policy)?.body())?;
        assert_eq!(body["tool_choice"], expected);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    }
    Ok(())
}

#[test]
fn disabled_reasoning_wire_matrix() -> TestResult {
    assert!(lower_reasoning(ReasoningPolicy::Disabled, ChatDisabledReasoningWire::Omit)?.is_none());
    let disabled = lower_reasoning(
        ReasoningPolicy::Disabled,
        ChatDisabledReasoningWire::EnabledFalse,
    )?
    .ok_or("disabled reasoning wire was omitted")?;
    assert_eq!(
        serde_json::to_value(disabled)?,
        serde_json::json!({"enabled": false})
    );
    let enabled = lower_reasoning(
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Required,
        },
        ChatDisabledReasoningWire::Omit,
    )?
    .ok_or("enabled reasoning wire was omitted")?;
    assert_eq!(
        serde_json::to_value(enabled)?,
        serde_json::json!({"effort": "medium", "exclude": false})
    );
    Ok(())
}

#[test]
fn disabled_reasoning_wire_lower_request_body() -> TestResult {
    let fixture = Fixture::new()?;
    let request = ModelRequest::builder(
        fixture.selected,
        fixture.effective,
        ConversationView::from_items(vec![item(
            41,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("code", None)?)],
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .reasoning(ReasoningPolicy::Disabled)
    .build()?;
    let plan = fixture
        .descriptor
        .validate_request(request, ValidationMode::Strict)?;
    let disabled_body: Value = serde_json::from_slice(
        lower_request(
            &plan,
            &fixture
                .policy(&plan)?
                .with_disabled_reasoning_wire(ChatDisabledReasoningWire::EnabledFalse),
        )?
        .body(),
    )?;
    assert_eq!(
        disabled_body["reasoning"],
        serde_json::json!({"enabled": false})
    );

    let enabled_request = ModelRequest::builder(
        fixture.selected,
        fixture.effective,
        ConversationView::from_items(vec![item(
            42,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("think", None)?)],
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(4))?,
            Timestamp::new(1_700_000_002, 0)?,
        ),
    )
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::High),
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let enabled_plan = fixture
        .descriptor
        .validate_request(enabled_request, ValidationMode::Strict)?;
    let enabled_policy = ChatRequestPolicy::new(
        ProtocolId::new("openrouter.chat-completions")?,
        ModelId::new("model/exact")?,
        enabled_plan.binding_digest(),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        fixture.selected,
        fixture.effective,
        512,
        ChatOutputLimitField::MaxCompletionTokens,
        ChatReasoningReplay::VisibleOnly,
        None,
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::High),
            requirement: CapabilityRequirement::Required,
        },
        crate::stream::ReasoningAliasPolicy::Reasoning,
    )?
    .with_disabled_reasoning_wire(ChatDisabledReasoningWire::EnabledFalse);
    let enabled_body: Value =
        serde_json::from_slice(lower_request(&enabled_plan, &enabled_policy)?.body())?;
    assert_eq!(
        enabled_body["reasoning"],
        serde_json::json!({"effort": "high", "exclude": false})
    );
    Ok(())
}

#[test]
fn enabled_cache_policy_is_outside_stateless_chat_v1() -> TestResult {
    use plakous_model::CapabilityRequirement;

    let fixture = Fixture::new()?;
    let request = ModelRequest::builder(
        fixture.selected,
        fixture.effective,
        ConversationView::from_items(vec![item(
            41,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("cached", None)?)],
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .tools(fixture.tools.clone())
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let plan = fixture
        .descriptor
        .validate_request(request, ValidationMode::Strict)?;
    let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
        return Err("managed cache was accepted by Chat V1".into());
    };
    assert_eq!(error.kind(), ChatRequestErrorKind::UnsupportedSemantic);
    assert!(error.message().contains("managed cache"));
    Ok(())
}

/// The main exchange classifies an enabled gateway plugin as a server
/// effect; only the hosted-retrieval side request has its own allowlist.
#[test]
fn main_classification_keeps_an_enabled_web_plugin_a_server_effect() -> TestResult {
    let side = crate::ChatRetrievalRequest::new("model-literal", "rust").body()?;
    assert_eq!(classify_replay_safety(&side), ReplaySafety::ServerEffect);
    assert_eq!(
        classify_replay_safety(
            br#"{"model":"m","plugins":[{"id":"context-compression","enabled":false}]}"#
        ),
        ReplaySafety::CostOnly
    );
    Ok(())
}
