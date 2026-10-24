use std::collections::BTreeMap;

use plakous_model::EntryId as TestEntryId;
use plakous_model::TextBlock as TestTextBlock;
use plakous_model::{
    AttemptId, BranchId, Capability, CapabilityConstraints, CapabilitySet, ConfiguredTool,
    ConversationItem, ConversationView, DataClassification, FactProvenance, FactSource,
    FeatureSupport, ModelAdapterProfileId, ModelCacheAffinity, ModelFamilyId, ModelRequest,
    Provenance, ReplayKindId, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId, SessionId,
    Support, Timestamp, ToolCallerDisposition, ToolConcurrency, ToolDefinition, ToolEffect,
    ToolExecutionDisposition, ToolInputContract, ToolName, ToolOutputContract, TurnId,
    ValidatedRequestPlan, ValidationMode,
};
use uuid::Uuid as TestUuid;

use super::super::*;
use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn selected_id(label: &[u8]) -> TestResult<SelectedProviderConfigurationId> {
    Ok(serde_json::from_value(serde_json::to_value(
        Digest::sha256(DigestDomain::SelectedProviderConfiguration, 5, &[label])?,
    )?)?)
}

fn effective_id(label: &[u8]) -> TestResult<EffectiveProviderConfigurationId> {
    Ok(serde_json::from_value(serde_json::to_value(
        Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[label])?,
    )?)?)
}

fn policy_with_authority(
    account: &[u8],
    selected: &[u8],
    effective: &[u8],
) -> TestResult<MessagesRequestPolicy> {
    Ok(MessagesRequestPolicy::new(
        ProtocolId::new("anthropic.messages")?,
        ModelId::new("claude-exact")?,
        Some(ModelId::new("claude-exact")?),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"descriptor"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"semantic-descriptor"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        selected_id(selected)?,
        effective_id(effective)?,
        8_192,
        None,
        "2023-06-01",
        "",
    ))
}

fn policy() -> TestResult<MessagesRequestPolicy> {
    policy_with_authority(b"account", b"selected", b"effective")
}

#[test]
fn policy_debug_never_prints_the_previous_message_id() -> TestResult {
    let policy = policy()?.with_claude_oauth_previous_message_id(Some("msg_secret_123".to_owned()));
    let debug = format!("{policy:?}");
    assert!(!debug.contains("msg_secret_123"), "{debug}");
    assert!(debug.contains("PreviousMessageId(..)"), "{debug}");
    Ok(())
}

#[test]
fn thinking_display_is_opt_in_and_never_applies_to_disabled_reasoning() -> TestResult {
    for display in [None, Some(MessagesThinkingDisplay::Updates)] {
        for (effort, expected_type) in [
            (Some(ReasoningEffort::Low), "adaptive"),
            (
                Some(ReasoningEffort::TokenBudget(
                    std::num::NonZeroU32::new(1024).ok_or("budget")?,
                )),
                "enabled",
            ),
        ] {
            let (thinking, _) = translate_reasoning(
                ReasoningPolicy::Enabled {
                    effort,
                    requirement: CapabilityRequirement::Required,
                },
                Some(1024),
                4096,
                false,
                display,
            )?;
            let body = serde_json::to_value(thinking)?;
            assert_eq!(body["type"], expected_type);
            if display.is_some() {
                assert_eq!(body["display"], "updates");
            } else {
                assert!(body.get("display").is_none());
            }
        }
        for omit_disabled in [false, true] {
            let (thinking, _) = translate_reasoning(
                ReasoningPolicy::Disabled,
                None,
                1024,
                omit_disabled,
                display,
            )?;
            assert_eq!(
                serde_json::to_value(thinking)?,
                if omit_disabled {
                    Value::Null
                } else {
                    serde_json::json!({"type":"disabled"})
                }
            );
        }
    }
    Ok(())
}

fn lowering_fixture() -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)> {
    lowering_fixture_with_history(vec![ConversationItem::new(
        TestEntryId::try_from(TestUuid::from_u128(4))?,
        Actor::User,
        vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
        Provenance::UserInput,
    )?])
}

fn lowering_fixture_with_history(
    history: Vec<ConversationItem>,
) -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)> {
    let selected = selected_id(b"selected")?;
    let effective = effective_id(b"effective")?;
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1_700_000_000, 0)?,
        None,
    );
    let features = [Capability::Streaming]
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
        ProtocolId::new("messages.test")?,
        RouteId::new("messages-test")?,
        ModelId::new("model/exact")?,
        None,
        None,
        ModelAdapterProfileId::new("messages-test")?,
        CapabilitySet::new(features, BTreeMap::new()),
    );
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(history)?,
        RequestMetadata::for_turn(
            RunId::try_from(TestUuid::from_u128(1))?,
            TurnId::try_from(TestUuid::from_u128(2))?,
            AttemptId::try_from(TestUuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .build()?;
    let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
    let policy = MessagesRequestPolicy::new(
        ProtocolId::new("messages.test")?,
        ModelId::new("model/exact")?,
        Some(ModelId::new("model/exact")?),
        plan.binding_digest(),
        descriptor.semantic_binding_digest()?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        selected,
        effective,
        8_192,
        None,
        "2023-06-01",
        "",
    );
    Ok((plan, policy))
}

fn lowering_fixture_with_customer_tool() -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)>
{
    let selected = selected_id(b"selected-tool")?;
    let effective = effective_id(b"effective-tool")?;
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1_700_000_000, 0)?,
        None,
    );
    let features = [Capability::Streaming, Capability::Tools]
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
        ProtocolId::new("messages.test")?,
        RouteId::new("messages-test")?,
        ModelId::new("model/exact")?,
        None,
        None,
        ModelAdapterProfileId::new("messages-test")?,
        CapabilitySet::new(features, BTreeMap::new()),
    );
    let tool = ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1",
            "lookup tool",
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
    );
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(4))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(TestUuid::from_u128(1))?,
            TurnId::try_from(TestUuid::from_u128(2))?,
            AttemptId::try_from(TestUuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .tools(vec![tool])
    .build()?;
    let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
    let policy = MessagesRequestPolicy::new(
        ProtocolId::new("messages.test")?,
        ModelId::new("model/exact")?,
        Some(ModelId::new("model/exact")?),
        plan.binding_digest(),
        descriptor.semantic_binding_digest()?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        selected,
        effective,
        8_192,
        None,
        "2023-06-01",
        "",
    );
    Ok((plan, policy))
}

fn lowering_fixture_with_tool_choice(
    tool_choice: ToolChoice,
) -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)> {
    let selected = selected_id(b"selected-tool-choice")?;
    let effective = effective_id(b"effective-tool-choice")?;
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1_700_000_000, 0)?,
        None,
    );
    let features = [Capability::Streaming, Capability::Tools]
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
        ProtocolId::new("messages.test")?,
        RouteId::new("messages-test")?,
        ModelId::new("model/exact")?,
        None,
        None,
        ModelAdapterProfileId::new("messages-test")?,
        CapabilitySet::new(features, BTreeMap::new()),
    );
    let tool = ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1",
            "lookup tool",
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
    );
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(4))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(TestUuid::from_u128(1))?,
            TurnId::try_from(TestUuid::from_u128(2))?,
            AttemptId::try_from(TestUuid::from_u128(3))?,
            Timestamp::new(1_700_000_001, 0)?,
        ),
    )
    .tools(vec![tool])
    .tool_choice(tool_choice)
    .build()?;
    let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
    let policy = MessagesRequestPolicy::new(
        ProtocolId::new("messages.test")?,
        ModelId::new("model/exact")?,
        Some(ModelId::new("model/exact")?),
        plan.binding_digest(),
        descriptor.semantic_binding_digest()?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        selected,
        effective,
        8_192,
        None,
        "2023-06-01",
        "",
    );
    Ok((plan, policy))
}

#[test]
fn tool_choice_none_with_tools_emits_none() -> TestResult {
    let (plan, policy) = lowering_fixture_with_tool_choice(ToolChoice::None)?;
    let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tool_choice"], serde_json::json!({"type": "none"}));
    Ok(())
}

#[test]
fn openrouter_gateway_session_emits_session_id_and_breakpoints_without_ttl() -> TestResult {
    let (plan, policy) = lowering_fixture_with_customer_tool()?;
    let session =
        Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"openrouter-session"])?.to_string();
    let lowered = lower_request(
        &plan,
        &policy
            .with_session_id(session.clone())
            .with_prompt_cache(true)
            .with_top_level_prompt_cache(false),
        ToolOwnership::default(),
    )?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["session_id"], session);
    assert!(body.get("cache_control").is_none());
    assert_eq!(
        body["tools"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert!(body["tools"][0]["cache_control"].get("ttl").is_none());
    let last_block = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .and_then(|blocks| blocks.last())
        .ok_or("last message content")?;
    assert_eq!(
        last_block["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert!(last_block["cache_control"].get("ttl").is_none());
    Ok(())
}

#[test]
fn forked_openrouter_session_id_does_not_reuse_source_identity() -> TestResult {
    let (plan, source_policy) = lowering_fixture_with_customer_tool()?;
    let (_, fork_policy) = lowering_fixture_with_customer_tool()?;
    let session = SessionId::try_from(TestUuid::from_u128(92_001))?;
    let source_aff = ModelCacheAffinity::from_runtime_entropy(
        session,
        BranchId::try_from(TestUuid::from_u128(92_002))?,
        0,
        [0x41; 32],
    )?;
    let fork_aff = ModelCacheAffinity::from_runtime_entropy(
        session,
        BranchId::try_from(TestUuid::from_u128(92_003))?,
        0,
        [0x42; 32],
    )?;
    let source = source_aff.identity().to_string();
    let fork = fork_aff.identity().to_string();
    assert_ne!(source, fork);
    let source_body: Value = serde_json::from_slice(
        lower_request(
            &plan,
            &source_policy
                .with_session_id(source.clone())
                .with_prompt_cache(true)
                .with_top_level_prompt_cache(false),
            ToolOwnership::default(),
        )?
        .body(),
    )?;
    let fork_body: Value = serde_json::from_slice(
        lower_request(
            &plan,
            &fork_policy
                .with_session_id(fork.clone())
                .with_prompt_cache(true)
                .with_top_level_prompt_cache(false),
            ToolOwnership::default(),
        )?
        .body(),
    )?;
    assert_eq!(source_body["session_id"], source);
    assert_eq!(fork_body["session_id"], fork);
    assert_ne!(source_body["session_id"], fork_body["session_id"]);
    assert!(source_body.get("cache_control").is_none());
    assert!(fork_body.get("cache_control").is_none());
    Ok(())
}

/// A harness-authored note in history reaches the wire as a user message.
///
/// The product writes one such note when it abandons a turn whose model
/// request was lost, and every later request on that session carries it.
/// This route refuses to translate a historical instruction actor, so the
/// note is spelled `User` with `Provenance::Instruction` — the pair the
/// product reads as harness-authored — rather than `System`. A note this
/// route cannot carry does not merely go missing: it fails every turn the
/// recovered session is asked for.
#[test]
fn a_harness_authored_note_in_history_lowers_as_a_user_message() -> TestResult {
    let (plan, policy) = lowering_fixture_with_history(vec![
        ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(4))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?,
        ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(5))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new(
                "the model request for this turn was abandoned",
                None,
            )?)],
            Provenance::Instruction,
        )?,
    ])?;
    let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    let messages = body["messages"]
        .as_array()
        .ok_or("lowered request has no messages")?;
    assert_eq!(messages.len(), 1, "consecutive user content is one message");
    assert_eq!(messages[0]["role"], "user");
    let text = serde_json::to_string(&messages[0]["content"])?;
    assert!(
        text.contains("was abandoned"),
        "the note did not reach the wire: {text}"
    );
    Ok(())
}

#[test]
fn seeded_content_filter_guidance_lowers_on_messages() -> TestResult {
    let instruction =
        "<content_filter_guidance>\nOffer a permitted alternative.\n</content_filter_guidance>";
    let (plan, policy) = lowering_fixture_with_history(vec![
        ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(4))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new(
                "original request",
                None,
            )?)],
            Provenance::UserInput,
        )?,
        ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(5))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new(instruction, None)?)],
            Provenance::Instruction,
        )?,
    ])?;
    let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(body["messages"][0]["role"], "user");
    let content = body["messages"][0]["content"]
        .as_array()
        .ok_or("no content")?;
    assert!(content.iter().any(|block| block["text"] == instruction));
    Ok(())
}

/// A historical instruction actor is still refused, on purpose.
///
/// This route has one system field and no place for a system turn inside
/// the conversation, so translating one silently would mean inventing a
/// position for it. The refusal is what sent the abandonment note to the
/// harness-authored spelling above.
#[test]
fn a_system_actor_in_history_is_refused_rather_than_invented() -> TestResult {
    let fixture = lowering_fixture_with_history(vec![ConversationItem::new(
        TestEntryId::try_from(TestUuid::from_u128(6))?,
        Actor::System,
        vec![ContentBlock::Text(TestTextBlock::new("a note", None)?)],
        Provenance::Instruction,
    )?]);
    let refused = match fixture {
        Err(_) => true,
        Ok((plan, policy)) => lower_request(&plan, &policy, ToolOwnership::default()).is_err(),
    };
    assert!(refused, "a system actor in history must not reach the wire");
    Ok(())
}

#[test]
fn openrouter_context_compression_plugin_is_disabled() -> TestResult {
    let (plan, policy) = lowering_fixture()?;
    let lowered = lower_request(
        &plan,
        &policy.with_plugins(OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS),
        ToolOwnership::default(),
    )?;
    let body: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(
        body["plugins"],
        serde_json::json!([{ "id": "context-compression", "enabled": false }])
    );
    Ok(())
}

#[test]
fn optional_gateway_router_is_exact_and_identity_bearing() -> TestResult {
    let (plan, policy) = lowering_fixture()?;
    let direct = lower_request(&plan, &policy, ToolOwnership::default())?;
    let routed_without_privacy = lower_request(
        &plan,
        &policy
            .clone()
            .with_router_policy(MessagesRouterPolicy::gateway_managed(None, None)?),
        ToolOwnership::default(),
    )?;
    let routed = lower_request(
        &plan,
        &policy
            .clone()
            .with_router_policy(MessagesRouterPolicy::gateway_managed(
                Some("deny".to_owned()),
                Some(true),
            )?),
        ToolOwnership::default(),
    )?;
    let direct_body: Value = serde_json::from_slice(direct.body())?;
    let routed_without_privacy_body: Value = serde_json::from_slice(routed_without_privacy.body())?;
    let routed_body: Value = serde_json::from_slice(routed.body())?;
    assert!(direct_body.get("provider").is_none());
    assert_eq!(
        routed_without_privacy_body["provider"],
        serde_json::json!({
            "allow_fallbacks": false,
            "require_parameters": true
        })
    );
    assert_eq!(
        routed_body["provider"],
        serde_json::json!({
            "allow_fallbacks": false,
            "require_parameters": true,
            "data_collection": "deny",
            "zdr": true
        })
    );
    assert_eq!(routed_body.as_object().map(serde_json::Map::len), Some(6));
    assert_ne!(
        direct.operation_digest(),
        routed_without_privacy.operation_digest()
    );
    assert_ne!(
        routed_without_privacy.operation_digest(),
        routed.operation_digest()
    );
    assert_ne!(direct.operation_digest(), routed.operation_digest());
    let without_privacy = policy
        .clone()
        .with_router_policy(MessagesRouterPolicy::gateway_managed(None, None)?);
    let with_privacy = policy
        .clone()
        .with_router_policy(MessagesRouterPolicy::gateway_managed(
            Some("deny".to_owned()),
            Some(true),
        )?);
    let with_privacy_false =
        policy
            .clone()
            .with_router_policy(MessagesRouterPolicy::gateway_managed(
                Some("deny".to_owned()),
                Some(false),
            )?);
    assert_ne!(
        messages_artifact_configuration_identity(&policy, plan.digest())?,
        messages_artifact_configuration_identity(&without_privacy, plan.digest())?
    );
    assert_ne!(
        messages_artifact_configuration_identity(&without_privacy, plan.digest())?,
        messages_artifact_configuration_identity(&with_privacy, plan.digest())?
    );
    assert_ne!(
        messages_artifact_configuration_identity(&with_privacy, plan.digest())?,
        messages_artifact_configuration_identity(&with_privacy_false, plan.digest())?
    );
    assert!(MessagesRouterPolicy::gateway_managed(Some("DENY".to_owned()), None).is_err());
    Ok(())
}

#[test]
fn omitted_router_preserves_anthropic_request_bytes() -> TestResult {
    let (plan, policy) = lowering_fixture()?;
    let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
    assert_eq!(
        lowered.body(),
        br#"{"model":"model/exact","max_tokens":8192,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"thinking":{"type":"disabled"},"stream":true}"#
    );
    Ok(())
}

#[test]
fn disabled_reasoning_omits_thinking_blocks_and_tool_choice_none() -> TestResult {
    let policy = policy()?;
    let thinking = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("hidden thought".to_owned()),
        Some(bind_replay_payload(
            &policy,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"sig\"")?,
        )?),
    )?;
    let (blocks, pending) = translate_assistant_bound(
        &[
            ContentBlock::Reasoning(thinking),
            ContentBlock::Text(plakous_model::TextBlock::new("bravo", None)?),
        ],
        &policy,
        &ToolOwnership::default(),
        false,
        false,
        &mut None,
        &mut crate::artifact::MessagesArtifactLineage::default(),
        &mut Vec::new(),
    )?;
    assert!(pending.is_empty());
    assert!(blocks.iter().all(|block| !matches!(
        block,
        MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
    )));
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            MessageBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["bravo"]);
    assert!(translate_tool_choice(&ToolChoice::None, false)?.is_none());
    assert!(matches!(
        translate_tool_choice(&ToolChoice::None, true)?,
        Some(ToolChoiceParam::None)
    ));
    Ok(())
}

fn raw(value: &str) -> TestResult<Box<RawValue>> {
    Ok(RawValue::from_string(value.to_owned())?)
}

#[allow(
    clippy::too_many_arguments,
    reason = "test mutates every closed envelope axis"
)]
fn rebound(
    source: &ReplayEnvelope,
    protocol: &str,
    model_family: Option<ModelFamilyId>,
    scope: ReplayScope,
    kind: &str,
    version: u32,
    classification: DataClassification,
) -> TestResult<ReplayEnvelope> {
    Ok(ReplayEnvelope::new(
        ProtocolId::new(protocol)?,
        model_family,
        scope,
        ReplayKindId::new(kind)?,
        version,
        classification,
        raw(source.payload().get())?,
    )?)
}

#[test]
fn signed_and_redacted_thinking_use_exact_canonical_replay_envelopes() -> TestResult {
    let policy = policy()?;
    let model = ModelId::new("claude-exact")?;
    for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
        let replay = bind_replay_payload(&policy, model.clone(), kind, raw("\"opaque\"")?)?;
        assert_eq!(replay.protocol(), policy.protocol());
        assert_eq!(replay.scope(), &ReplayScope::ExactModel(model.clone()));
        assert_eq!(replay.kind().as_str(), kind);
        assert_eq!(replay.version(), MESSAGES_REPLAY_SCHEMA_REVISION);
        assert_eq!(replay.classification(), DataClassification::Sensitive);
        assert_eq!(replay_payload(&replay, &policy, kind)?.get(), "\"opaque\"");
        let parsed: Value = serde_json::from_str(replay.payload().get())?;
        let recanonical = SemanticJsonV1::from_value(parsed)?;
        assert_eq!(
            replay.payload().get().as_bytes(),
            recanonical.canonical_bytes().as_ref(),
            "{kind} payload must be JCS so history persist can roundtrip the digest"
        );
    }
    Ok(())
}

#[test]
fn empty_signature_replay_converges_to_portable_visible_reasoning() -> TestResult {
    let policy = policy()?;
    let replay = bind_replay_payload(
        &policy,
        ModelId::new("claude-exact")?,
        SIGNATURE_REPLAY_KIND,
        raw("\"\"")?,
    )?;
    let reasoning = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("portable thought".to_owned()),
        Some(replay),
    )?;
    assert!(matches!(
        native_or_portable_thinking(
            &reasoning,
            &policy,
            SIGNATURE_REPLAY_KIND,
            true,
            &mut Vec::new(),
        ),
        Some(MessageBlock::Text { text, .. }) if text == "portable thought"
    ));
    Ok(())
}

#[test]
fn replay_ignores_freshness_binding_but_requires_semantic_descriptor() -> TestResult {
    let original = policy()?;
    let mut refreshed = original.clone();
    refreshed.descriptor_binding =
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"fresh-observation"])?;
    for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
        let replay = bind_replay_payload(
            &original,
            ModelId::new("claude-exact")?,
            kind,
            raw("\"opaque\"")?,
        )?;
        assert_eq!(
            replay_payload(&replay, &refreshed, kind)?.get(),
            "\"opaque\""
        );
        let mut changed = refreshed.clone();
        changed.semantic_descriptor_binding =
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"changed-capability"])?;
        assert!(replay_payload(&replay, &changed, kind).is_err());
    }
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one replay fixture exercises completed and active history boundaries"
)]
fn completed_thinking_projects_changed_bindings_but_active_turn_stays_exact() -> TestResult {
    let source = policy()?.with_strict_thinking_replay(true);
    let mut effort_target = source.clone();
    effort_target.effective_provider_configuration = effective_id(b"lower-effort")?;
    let mut model_target = source.clone();
    model_target.requested_model = ModelId::new("claude-successor")?;
    model_target.expected_reported_model = Some(ModelId::new("claude-successor")?);
    model_target.semantic_descriptor_binding =
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"successor-descriptor"])?;
    let mut account_target = source.clone();
    account_target.account_binding =
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"new-account"])?;
    let mut beta_target = source.clone();
    beta_target.beta_header = "new-beta".into();

    let mut transition_sets = Vec::new();
    for (kind, visibility) in [
        (SIGNATURE_REPLAY_KIND, ReasoningVisibility::Visible),
        (REDACTED_REPLAY_KIND, ReasoningVisibility::Redacted),
    ] {
        let reasoning = plakous_model::ReasoningBlock::new(
            visibility,
            (visibility == ReasoningVisibility::Visible).then(|| "private thought".into()),
            Some(bind_replay_payload(
                &source,
                ModelId::new("claude-exact")?,
                kind,
                raw("\"signature\"")?,
            )?),
        )?;
        let assistant = ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(101))?,
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(reasoning),
                ContentBlock::Text(TestTextBlock::new("answer", None)?),
            ],
            Provenance::ModelOutput,
        )?;
        let user = ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(102))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new("next", None)?)],
            Provenance::UserInput,
        )?;
        let initial_user = ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(100))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new("first", None)?)],
            Provenance::UserInput,
        )?;
        let mut transition_ids = std::collections::HashSet::new();
        for (target, expected_native_count) in [
            (&source, 1),
            (&effort_target, 0),
            (&model_target, 0),
            (&account_target, 0),
            (&beta_target, 0),
        ] {
            let history = ConversationView::from_items(vec![
                initial_user.clone(),
                assistant.clone(),
                user.clone(),
            ])?;
            let mut diagnostics = Vec::new();
            let (messages, _, projections) = translate_history(
                &history,
                &target
                    .clone()
                    .with_completed_history_prefix(Some(history.semantic_prefix_identity(2)?)),
                &ToolOwnership::default(),
                false,
                true,
                &mut diagnostics,
            )?;
            let native_count = messages[1]
                .content
                .iter()
                .filter(|block| {
                    matches!(
                        block,
                        MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
                    )
                })
                .count();
            assert_eq!(native_count, expected_native_count);
            assert_eq!(messages[1].content.len(), native_count + 1);
            assert!(
                matches!(&messages[1].content[native_count], MessageBlock::Text { text, .. } if text == "answer")
            );
            assert_eq!(diagnostics, [] as [String; 0]);
            assert_eq!(projections.len(), usize::from(native_count == 0));
            if native_count == 0 {
                let identity = projections[0]
                    .history_projection_identity()
                    .ok_or("missing identity")?;
                assert!(
                    transition_ids.insert(identity),
                    "distinct targets shared an identity"
                );
                let wire = serde_json::to_string(&messages)?;
                assert!(!wire.contains("private thought"));
                assert!(!wire.contains("signature"));
                let quiescent =
                    ConversationView::from_items(vec![initial_user.clone(), assistant.clone()])?;
                assert!(
                    translate_history(
                        &quiescent,
                        &target.clone().with_completed_history_prefix(Some(
                            quiescent.semantic_prefix_identity(2)?
                        )),
                        &ToolOwnership::default(),
                        false,
                        true,
                        &mut Vec::new(),
                    )
                    .is_ok()
                );
                // Identical actor shapes can be an assistant continuation
                // or a live steering boundary. Neither proves a closed turn.
                for unproven in [&quiescent, &history] {
                    assert!(
                        translate_history(
                            unproven,
                            target,
                            &ToolOwnership::default(),
                            false,
                            true,
                            &mut Vec::new(),
                        )
                        .is_err()
                    );
                }
                // A proof for another exact projection is never reusable.
                let foreign = target
                    .clone()
                    .with_completed_history_prefix(Some(history.semantic_prefix_identity(3)?));
                assert!(
                    translate_history(
                        &quiescent,
                        &foreign,
                        &ToolOwnership::default(),
                        false,
                        true,
                        &mut Vec::new(),
                    )
                    .is_err()
                );
                let tool_call = ToolCallBlock::new(
                    CallId::try_from(TestUuid::from_u128(103))?,
                    ToolName::new("process_session_start")?,
                    ToolExecutionOwner::Local,
                    plakous_model::ToolCallState::Requested,
                    plakous_model::ToolCorrelation::new(
                        Some("call_current".into()),
                        None,
                        ToolCallerLineage::Direct,
                    )?,
                    r#"{"command":"pwd"}"#,
                )?;
                let mut active_blocks = assistant.blocks().to_vec();
                active_blocks.push(ContentBlock::ToolCall(tool_call.clone()));
                let active_assistant = ConversationItem::new(
                    TestEntryId::try_from(TestUuid::from_u128(104))?,
                    Actor::Assistant,
                    active_blocks,
                    Provenance::ModelOutput,
                )?;
                let active =
                    ConversationView::from_items(vec![user.clone(), active_assistant.clone()])?;
                assert!(
                    translate_history(
                        &active,
                        target,
                        &ToolOwnership::default(),
                        false,
                        true,
                        &mut Vec::new(),
                    )
                    .is_err()
                );
                let cancelled_result = ConversationItem::new(
                    TestEntryId::try_from(TestUuid::from_u128(105))?,
                    Actor::Tool,
                    vec![ContentBlock::ToolResult(
                        plakous_model::ToolResultBlock::new(
                            tool_call.call_id(),
                            tool_call.owner().clone(),
                            ToolResultKind::Failure,
                            vec![ToolResultPart::Text(TestTextBlock::new(
                                "cancelled after joined cleanup",
                                None,
                            )?)],
                            tool_call.correlation().clone(),
                        )?,
                    )],
                    Provenance::ToolExecution,
                )?;
                let cancelled = ConversationView::from_items(vec![
                    user.clone(),
                    active_assistant,
                    cancelled_result,
                ])?;
                let closed = target
                    .clone()
                    .with_completed_history_prefix(Some(cancelled.semantic_prefix_identity(3)?));
                let (messages, _, repeated_projections) = translate_history(
                    &cancelled,
                    &closed,
                    &ToolOwnership::default(),
                    false,
                    true,
                    &mut Vec::new(),
                )?;
                assert_eq!(messages.len(), 3);
                assert_eq!(projections, repeated_projections);
                assert!(!messages[1].content.iter().any(|block| matches!(
                    block,
                    MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
                )));
            }
        }
        transition_sets.push(transition_ids);
    }
    assert_eq!(
        transition_sets[0], transition_sets[1],
        "replay kind is not a transition fact"
    );
    Ok(())
}

#[test]
fn historical_projection_still_rejects_malformed_and_unfinished_history() -> TestResult {
    let source = policy()?;
    let mut target = source.clone().with_strict_thinking_replay(true);
    target.effective_provider_configuration = effective_id(b"new-effort")?;
    let replay = bind_replay_payload(
        &source,
        ModelId::new("claude-exact")?,
        SIGNATURE_REPLAY_KIND,
        raw("\"secret\"")?,
    )?;
    let mut malformed: Value = serde_json::from_str(replay.payload().get())?;
    malformed["value"] = serde_json::json!({"not": "a signature string"});
    for (payload, unfinished) in [
        (serde_json::to_string(&malformed)?, false),
        (replay.payload().get().into(), true),
    ] {
        let carried = ReplayEnvelope::new(
            replay.protocol().clone(),
            None,
            replay.scope().clone(),
            replay.kind().clone(),
            replay.version(),
            replay.classification(),
            raw(&payload)?,
        )?;
        let mut blocks = vec![ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Visible,
            Some("private thought".into()),
            Some(carried),
        )?)];
        if unfinished {
            blocks.push(batch_call(1)?);
        }
        let history =
            ConversationView::from_items(vec![batch_item(100, Actor::Assistant, blocks)?])?;
        let closed = target
            .clone()
            .with_completed_history_prefix(Some(history.semantic_prefix_identity(1)?));
        let mut diagnostics = Vec::new();
        let Err(error) = translate_history(
            &history,
            &closed,
            &ToolOwnership::default(),
            false,
            true,
            &mut diagnostics,
        ) else {
            return Err("malformed or unfinished history was accepted".into());
        };
        assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
        assert_eq!(diagnostics, [] as [String; 0]);
    }
    Ok(())
}

#[test]
fn historical_projection_identity_excludes_payload_and_freshness() -> TestResult {
    let source = policy()?;
    let mut target = source.clone();
    target.effective_provider_configuration = effective_id(b"new-effort")?;
    let replay = bind_replay_payload(
        &source,
        ModelId::new("claude-exact")?,
        SIGNATURE_REPLAY_KIND,
        raw("\"first-secret\"")?,
    )?;
    let identity = historical_projection_identity(&replay, &target)?;
    let other_payload = bind_replay_payload(
        &source,
        ModelId::new("claude-exact")?,
        REDACTED_REPLAY_KIND,
        raw("\"second-secret\"")?,
    )?;
    target.descriptor_binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"freshness"])?;
    target.completed_history_prefix = None;
    assert_eq!(
        identity,
        historical_projection_identity(&other_payload, &target)?
    );
    let mut other_source = source.clone();
    other_source.expected_reported_model = Some(ModelId::new("claude-other-actual")?);
    let other_actual = bind_replay_payload(
        &other_source,
        ModelId::new("claude-other-actual")?,
        SIGNATURE_REPLAY_KIND,
        raw("\"first-secret\"")?,
    )?;
    assert_ne!(
        identity,
        historical_projection_identity(&other_actual, &target)?
    );
    target.expected_reported_model = Some(ModelId::new("claude-other-actual")?);
    assert_ne!(identity, historical_projection_identity(&replay, &target)?);
    Ok(())
}

#[test]
fn bound_thinking_replay_survives_history_segment_codec_roundtrip() -> TestResult {
    use plakous_model::{
        ConversationItem, EntryId, HistorySegment, HistorySegmentKind, Provenance, ReasoningBlock,
    };
    use uuid::Uuid;

    let policy = policy()?;
    let model = ModelId::new("claude-exact")?;
    for signature in ["\"opaque\"", "\"\""] {
        let replay = bind_replay_payload(
            &policy,
            model.clone(),
            SIGNATURE_REPLAY_KIND,
            raw(signature)?,
        )?;
        let segment = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(1))?,
                Actor::Assistant,
                vec![ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Omitted,
                    None,
                    Some(replay),
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let mut value: Value = serde_json::from_slice(segment.canonical_bytes())?;
        let object = value
            .as_object_mut()
            .ok_or("history segment canonical bytes were not an object")?;
        if object
            .insert(
                "identity".to_owned(),
                serde_json::to_value(segment.identity())?,
            )
            .is_some()
        {
            return Err("canonical history bytes already carried identity".into());
        }
        let roundtrip: HistorySegment = serde_json::from_value(value)?;
        assert_eq!(roundtrip.identity(), segment.identity(), "{signature}");
        assert_eq!(
            roundtrip.canonical_bytes(),
            segment.canonical_bytes(),
            "{signature}"
        );
    }
    Ok(())
}

#[test]
fn replay_outer_binding_rejects_every_one_field_mutation_for_both_kinds() -> TestResult {
    let policy = policy()?;
    let model = ModelId::new("claude-exact")?;
    for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
        let source = bind_replay_payload(&policy, model.clone(), kind, raw("\"opaque\"")?)?;
        let foreign_kind = if kind == SIGNATURE_REPLAY_KIND {
            REDACTED_REPLAY_KIND
        } else {
            SIGNATURE_REPLAY_KIND
        };
        let variants = [
            rebound(
                &source,
                "foreign.messages",
                None,
                ReplayScope::ExactModel(model.clone()),
                kind,
                MESSAGES_REPLAY_SCHEMA_REVISION,
                DataClassification::Sensitive,
            )?,
            rebound(
                &source,
                "anthropic.messages",
                Some(ModelFamilyId::new("foreign-family")?),
                ReplayScope::ExactModel(model.clone()),
                kind,
                MESSAGES_REPLAY_SCHEMA_REVISION,
                DataClassification::Sensitive,
            )?,
            rebound(
                &source,
                "anthropic.messages",
                None,
                ReplayScope::ExactModel(ModelId::new("claude-foreign")?),
                kind,
                MESSAGES_REPLAY_SCHEMA_REVISION,
                DataClassification::Sensitive,
            )?,
            rebound(
                &source,
                "anthropic.messages",
                None,
                ReplayScope::Protocol,
                kind,
                MESSAGES_REPLAY_SCHEMA_REVISION,
                DataClassification::Sensitive,
            )?,
            rebound(
                &source,
                "anthropic.messages",
                None,
                ReplayScope::ExactModel(model.clone()),
                kind,
                MESSAGES_REPLAY_SCHEMA_REVISION - 1,
                DataClassification::Sensitive,
            )?,
            rebound(
                &source,
                "anthropic.messages",
                None,
                ReplayScope::ExactModel(model.clone()),
                kind,
                MESSAGES_REPLAY_SCHEMA_REVISION,
                DataClassification::Public,
            )?,
            rebound(
                &source,
                "anthropic.messages",
                None,
                ReplayScope::ExactModel(model.clone()),
                foreign_kind,
                MESSAGES_REPLAY_SCHEMA_REVISION,
                DataClassification::Sensitive,
            )?,
        ];
        for replay in variants {
            assert!(!projectable_historical_thinking(&replay, &policy, kind));
            let Err(error) = replay_payload(&replay, &policy, kind) else {
                return Err("foreign outer replay axis was accepted".into());
            };
            assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
        }
    }
    Ok(())
}

#[test]
fn replay_inner_binding_rejects_every_one_field_mutation_for_both_kinds() -> TestResult {
    let policy = policy()?;
    let model = ModelId::new("claude-exact")?;
    for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
        let source = bind_replay_payload(&policy, model.clone(), kind, raw("\"opaque\"")?)?;
        let base: Value = serde_json::from_str(source.payload().get())?;
        let mut identities = std::collections::HashSet::new();
        identities.insert(historical_projection_identity(&source, &policy)?);
        let mutations = [
            ("requested_model", serde_json::json!("claude-foreign")),
            ("actual_model", serde_json::json!("claude-foreign")),
            (
                "semantic_descriptor_binding",
                serde_json::to_value(Digest::sha256(
                    DigestDomain::ModelBinding,
                    1,
                    &[b"foreign-descriptor"],
                )?)?,
            ),
            (
                "account_binding",
                serde_json::to_value(Digest::sha256(
                    DigestDomain::ModelBinding,
                    1,
                    &[b"foreign-account"],
                )?)?,
            ),
            (
                "selected_provider_configuration",
                serde_json::to_value(selected_id(b"foreign-selected")?)?,
            ),
            (
                "effective_provider_configuration",
                serde_json::to_value(effective_id(b"foreign-effective")?)?,
            ),
            (
                "replay_schema_revision",
                serde_json::json!(MESSAGES_REPLAY_SCHEMA_REVISION + 1),
            ),
            ("api_version", serde_json::json!("2099-01-01")),
            ("beta_header", serde_json::json!("foreign-beta")),
        ];
        for (field, replacement) in mutations {
            let mut changed = base.clone();
            changed[field] = replacement;
            let replay = ReplayEnvelope::new(
                source.protocol().clone(),
                None,
                source.scope().clone(),
                source.kind().clone(),
                source.version(),
                source.classification(),
                raw(&serde_json::to_string(&changed)?)?,
            )?;
            let Err(error) = replay_payload(&replay, &policy, kind) else {
                return Err(format!("foreign inner replay axis was accepted: {field}").into());
            };
            assert_eq!(
                error.kind(),
                MessagesRequestErrorKind::InvalidRequest,
                "{field}"
            );
            if matches!(field, "actual_model" | "replay_schema_revision") {
                assert!(!projectable_historical_thinking(&replay, &policy, kind));
            } else {
                assert!(projectable_historical_thinking(&replay, &policy, kind));
                assert!(
                    identities.insert(historical_projection_identity(&replay, &policy)?),
                    "{field}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn replay_debug_and_errors_redact_signed_and_redacted_payloads() -> TestResult {
    let policy = policy()?;
    for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
        let sentinel = format!("opaque-secret-{kind}");
        let replay = bind_replay_payload(
            &policy,
            ModelId::new("claude-exact")?,
            kind,
            raw(&serde_json::to_string(&sentinel)?)?,
        )?;
        assert!(!format!("{replay:?}").contains(&sentinel));
        let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?;
        let Err(error) = replay_payload(&replay, &foreign, kind) else {
            return Err("foreign replay account was accepted".into());
        };
        assert!(!error.to_string().contains(&sentinel));
        assert!(error.to_string().len() < 128);
    }
    Ok(())
}

#[test]
fn foreign_summary_and_opaque_reasoning_project_portably() -> TestResult {
    let policy = policy()?;
    let summary = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Summary,
        Some("portable reasoning summary".to_owned()),
        None,
    )?;
    let opaque = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::OpaqueReplay,
        None,
        Some(ReplayEnvelope::new(
            ProtocolId::new("openai.codex.responses")?,
            Some(ModelFamilyId::new("gpt")?),
            ReplayScope::ExactModel(ModelId::new("gpt-5.6-sol")?),
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            raw("\"encrypted\"")?,
        )?),
    )?;
    let visible_without_replay = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("displayable thought".to_owned()),
        None,
    )?;
    let (blocks, pending) = translate_assistant(
        &[
            ContentBlock::Text(plakous_model::TextBlock::new(
                "prior answer",
                Some(plakous_model::TextPhase::Final),
            )?),
            ContentBlock::Reasoning(summary),
            ContentBlock::Reasoning(opaque),
            ContentBlock::Reasoning(visible_without_replay),
        ],
        &policy,
    )?;
    assert!(pending.is_empty());
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            MessageBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        [
            "prior answer",
            "portable reasoning summary",
            "displayable thought"
        ]
    );
    assert!(blocks.iter().all(|block| !matches!(
        block,
        MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
    )));

    let native = bind_replay_payload(
        &policy,
        ModelId::new("claude-exact")?,
        SIGNATURE_REPLAY_KIND,
        raw("\"opaque\"")?,
    )?;
    let visible = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("same-family thought".to_owned()),
        Some(native),
    )?;
    let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?;
    let (folded, pending) = translate_assistant(&[ContentBlock::Reasoning(visible)], &foreign)?;
    assert!(pending.is_empty());
    assert!(matches!(
        folded.as_slice(),
        [MessageBlock::Text { text, .. }] if text == "same-family thought"
    ));
    Ok(())
}

fn foreign_responses_opaque(fallback: OpaqueFallback) -> TestResult<ContentBlock> {
    Ok(ContentBlock::Opaque(plakous_model::OpaqueBlock::new(
        ReplayEnvelope::new(
            ProtocolId::new("openai.codex.responses")?,
            Some(ModelFamilyId::new("gpt")?),
            ReplayScope::ExactModel(ModelId::new("gpt-5.6-sol")?),
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            raw("\"encrypted\"")?,
        )?,
        fallback,
    )?))
}

#[test]
fn foreign_opaque_siblings_project_portably() -> TestResult {
    let policy = policy()?;
    let (text_opaque, pending) = translate_assistant(
        &[
            ContentBlock::Text(plakous_model::TextBlock::new(
                "prior answer",
                Some(plakous_model::TextPhase::Final),
            )?),
            foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
        ],
        &policy,
    )?;
    assert!(pending.is_empty());
    assert!(matches!(
        text_opaque.as_slice(),
        [MessageBlock::Text { text, .. }] if text == "prior answer"
    ));

    let (commentary_opaque, pending) = translate_assistant(
        &[
            ContentBlock::Text(plakous_model::TextBlock::new(
                "live comment",
                Some(plakous_model::TextPhase::Commentary),
            )?),
            foreign_responses_opaque(OpaqueFallback::Omit)?,
        ],
        &policy,
    )?;
    assert!(pending.is_empty());
    assert!(matches!(
        commentary_opaque.as_slice(),
        [MessageBlock::Text { text, .. }] if text == "live comment"
    ));

    let call = ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(11))?,
        ToolName::new("process_session_start")?,
        ToolExecutionOwner::Local,
        plakous_model::ToolCallState::Requested,
        plakous_model::ToolCorrelation::new(
            Some("call_1".into()),
            None,
            ToolCallerLineage::Direct,
        )?,
        "{\"command\":\"pwd\"}",
    )?;
    let (tool_opaque, pending) = translate_assistant(
        &[
            ContentBlock::ToolCall(call),
            foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
        ],
        &policy,
    )?;
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        tool_opaque.as_slice(),
        [MessageBlock::ToolUse { id, name, .. }]
            if id == "call_1" && name == "process_session_start"
    ));
    Ok(())
}

/// Replay sends the exact name the model called, never the canonical
/// name the call resolved to.
#[test]
fn an_aliased_call_replays_its_exposed_name() -> TestResult {
    let policy = policy()?;
    let call = ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(12))?,
        ToolName::new("Bash")?,
        ToolExecutionOwner::Local,
        plakous_model::ToolCallState::Requested,
        plakous_model::ToolCorrelation::new(
            Some("call_2".into()),
            None,
            ToolCallerLineage::Direct,
        )?,
        "{\"command\":\"pwd\"}",
    )?
    .with_canonical_name(ToolName::new("process_session_start")?);
    let (blocks, pending) = translate_assistant(&[ContentBlock::ToolCall(call)], &policy)?;
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        blocks.as_slice(),
        [MessageBlock::ToolUse { id, name, .. }] if id == "call_2" && name == "Bash"
    ));
    Ok(())
}

/// Rebind re-projection onto Messages: a Codex Code Mode
/// `exec` cell (UTF-8 input, canonical as a JSON string literal) becomes a
/// `tool_use` whose object input wraps the program as `{"input": ...}`;
/// a call whose text never parsed as JSON wraps its raw text the same way.
#[test]
fn code_mode_exec_cell_replays_as_an_object_wrapped_tool_use() -> TestResult {
    let policy = policy()?;
    let program = "const out = await tools.exec_command({cmd: 'pwd'});\nyield_(out);";
    let exec_call = ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(21))?,
        ToolName::new("exec")?,
        ToolExecutionOwner::Local,
        plakous_model::ToolCallState::Requested,
        plakous_model::ToolCorrelation::new(
            Some("call_exec_1".into()),
            None,
            ToolCallerLineage::Direct,
        )?,
        serde_json::to_string(program)?,
    )?;
    let (blocks, pending) = translate_assistant(&[ContentBlock::ToolCall(exec_call)], &policy)?;
    assert_eq!(pending.len(), 1);
    let [
        MessageBlock::ToolUse {
            id, name, input, ..
        },
    ] = blocks.as_slice()
    else {
        return Err(format!("expected one tool_use, got {blocks:?}").into());
    };
    assert_eq!(id, "call_exec_1");
    assert_eq!(name, "exec");
    assert_eq!(input, &serde_json::json!({ "input": program }));

    let unparsed = ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(22))?,
        ToolName::new("exec")?,
        ToolExecutionOwner::Local,
        plakous_model::ToolCallState::Requested,
        plakous_model::ToolCorrelation::new(
            Some("call_exec_2".into()),
            None,
            ToolCallerLineage::Direct,
        )?,
        "not json at all",
    )?;
    assert!(unparsed.arguments().is_none());
    let (blocks, _) = translate_assistant(&[ContentBlock::ToolCall(unparsed)], &policy)?;
    let [MessageBlock::ToolUse { input, .. }] = blocks.as_slice() else {
        return Err(format!("expected one tool_use, got {blocks:?}").into());
    };
    assert_eq!(input, &serde_json::json!({ "input": "not json at all" }));
    Ok(())
}

#[test]
fn local_programmatic_lineage_is_rejected_at_provider_wire_boundary() -> TestResult {
    let call = ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(12))?,
        ToolName::new("local_lookup")?,
        ToolExecutionOwner::Local,
        plakous_model::ToolCallState::Requested,
        plakous_model::ToolCorrelation::new(
            Some("call_local_1".into()),
            None,
            ToolCallerLineage::local_programmatic(
                91,
                4,
                "cell-4",
                7,
                CallId::try_from(uuid::Uuid::from_u128(12))?,
                1,
            )?,
        )?,
        r#"{"query":"local"}"#,
    )?;
    let Err(error) = translate_assistant(&[ContentBlock::ToolCall(call)], &policy()?) else {
        return Err("local lineage translated to provider wire".into());
    };
    assert!(error.to_string().contains("local programmatic caller"));
    Ok(())
}

#[test]
fn foreign_citation_refusal_and_opaque_summary_project_portably() -> TestResult {
    let policy = policy()?;
    let citation = plakous_model::CitationBlock::new(
        plakous_model::CitationTarget::new(
            plakous_model::EntryId::try_from(uuid::Uuid::from_u128(3))?,
            0,
        ),
        plakous_model::CitationSource::new("src", "ann", None, None)?,
        None,
    );
    let (cited, pending) = translate_assistant(
        &[
            ContentBlock::Text(plakous_model::TextBlock::new("cited", None)?),
            ContentBlock::Citation(citation),
            foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
        ],
        &policy,
    )?;
    assert!(pending.is_empty());
    assert!(matches!(
        cited.as_slice(),
        [MessageBlock::Text { text, .. }] if text == "cited"
    ));

    let (refused, pending) = translate_assistant(
        &[ContentBlock::Refusal(plakous_model::RefusalBlock::new(
            "cannot",
        )?)],
        &policy,
    )?;
    assert!(pending.is_empty());
    assert!(matches!(
        refused.as_slice(),
        [MessageBlock::Text { text, .. }] if text == "cannot"
    ));

    let (summarized, pending) = translate_assistant(
        &[foreign_responses_opaque(OpaqueFallback::SemanticSummary(
            "portable opaque summary".into(),
        ))?],
        &policy,
    )?;
    assert!(pending.is_empty());
    assert!(matches!(
        summarized.as_slice(),
        [MessageBlock::Text { text, .. }] if text == "portable opaque summary"
    ));

    let (opaque_only, pending) = translate_assistant(
        &[foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?],
        &policy,
    )?;
    assert!(pending.is_empty());
    assert_eq!(opaque_only, [] as [MessageBlock; 0]);

    let (compacted, pending) = translate_assistant(
        &[ContentBlock::Compaction(
            plakous_model::CompactionBlock::new("earlier turns were compacted")?,
        )],
        &policy,
    )?;
    assert!(pending.is_empty());
    assert!(matches!(
        compacted.as_slice(),
        [MessageBlock::Text { text, .. }] if text == "earlier turns were compacted"
    ));

    Ok(())
}

#[test]
fn native_compaction_refuses_messages_lowering() -> TestResult {
    let policy = policy()?;
    let native = plakous_model::CompactionBlock::new("ciphertext placeholder")?
        .with_native_item("{\"type\":\"compaction\",\"encrypted_content\":\"x\"}".into());
    let error = translate_assistant(&[ContentBlock::Compaction(native)], &policy)
        .err()
        .ok_or("Messages must not send native ciphertext as text")?;
    assert_eq!(error.kind(), MessagesRequestErrorKind::ProtectedHistory);
    assert!(!error.message().contains("ciphertext placeholder"));
    Ok(())
}

#[test]
fn replay_rebind_and_configuration_mismatches_refuse_both_kinds() -> TestResult {
    let source_policy = policy()?;
    let foreign_policies = [
        policy_with_authority(b"foreign-account", b"selected", b"effective")?,
        policy_with_authority(b"account", b"foreign-selected", b"effective")?,
        policy_with_authority(b"account", b"selected", b"foreign-effective")?,
    ];
    for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
        let replay = bind_replay_payload(
            &source_policy,
            ModelId::new("claude-exact")?,
            kind,
            raw("\"opaque\"")?,
        )?;
        for foreign in &foreign_policies {
            if replay_payload(&replay, foreign, kind).is_ok() {
                return Err("rebind or configuration mismatch reused replay".into());
            }
        }
    }
    Ok(())
}

#[test]
fn incompatible_present_replay_emits_bounded_diagnostic() -> TestResult {
    let source = policy()?;
    let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?;
    for (kind, visibility) in [
        (SIGNATURE_REPLAY_KIND, ReasoningVisibility::Visible),
        (REDACTED_REPLAY_KIND, ReasoningVisibility::Redacted),
    ] {
        let replay = bind_replay_payload(
            &source,
            ModelId::new("claude-exact")?,
            kind,
            raw("\"opaque\"")?,
        )?;
        let reasoning = plakous_model::ReasoningBlock::new(
            visibility,
            (visibility != ReasoningVisibility::Redacted).then(|| "portable".into()),
            Some(replay),
        )?;
        let mut diagnostics = Vec::new();
        let block = native_or_portable_thinking(
            &reasoning,
            &foreign,
            kind,
            visibility != ReasoningVisibility::Redacted,
            &mut diagnostics,
        );
        if visibility == ReasoningVisibility::Redacted {
            assert!(block.is_none());
        } else {
            assert!(matches!(block, Some(MessageBlock::Text { text, .. }) if text == "portable"));
        }
        assert_eq!(
            diagnostics,
            ["incompatible Messages thinking replay was stripped"]
        );
    }
    Ok(())
}

/// Under strict replay, reasoning that cannot be replayed natively is
/// dropped. It is never flattened into assistant text, whatever is wrong
/// with it.
///
/// Anthropic's `reasoning_extraction` classifier refused a resumed Claude
/// Opus 5 turn that received the model's own chain of thought back as text.
#[test]
fn strict_replay_drops_unreplayable_reasoning_instead_of_sending_it_as_text() -> TestResult {
    let source = policy()?;
    let strict = policy()?.with_strict_thinking_replay(true);
    let lenient = policy()?;
    let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?
        .with_strict_thinking_replay(true);

    // No signature at all: the shape a restored turn has when the envelope
    // did not survive persistence.
    let bare = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("private chain of thought".into()),
        None,
    )?;
    let mut diagnostics = Vec::new();
    assert!(
        native_or_portable_thinking(
            &bare,
            &strict,
            SIGNATURE_REPLAY_KIND,
            true,
            &mut diagnostics
        )
        .is_none()
    );
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].contains("never sent as text"));
    // Lenient lowering is unchanged, so nothing else on this wire moves.
    let mut lenient_diagnostics = Vec::new();
    assert!(matches!(
        native_or_portable_thinking(
            &bare,
            &lenient,
            SIGNATURE_REPLAY_KIND,
            true,
            &mut lenient_diagnostics
        ),
        Some(MessageBlock::Text { text, .. }) if text == "private chain of thought"
    ));
    assert_eq!(lenient_diagnostics, [] as [String; 0]);

    // A signature from another protocol is refused the same way.
    let foreign_replay = bind_replay_payload(
        &source,
        ModelId::new("claude-exact")?,
        SIGNATURE_REPLAY_KIND,
        raw("\"opaque\"")?,
    )?;
    let carried = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("private chain of thought".into()),
        Some(foreign_replay),
    )?;
    let mut diagnostics = Vec::new();
    assert!(
        native_or_portable_thinking(
            &carried,
            &foreign,
            SIGNATURE_REPLAY_KIND,
            true,
            &mut diagnostics
        )
        .is_none()
    );
    assert!(diagnostics[0].contains("never sent as text"));

    // A summary has no native Messages form, so it is dropped too.
    let summary = plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Summary,
        Some("a summary of the private chain of thought".into()),
        None,
    )?;
    let mut diagnostics = Vec::new();
    assert!(translate_reasoning_block(&summary, &strict, &mut diagnostics).is_none());
    assert!(diagnostics[0].contains("never sent as text"));
    Ok(())
}

#[test]
fn update_after_terminal_result_is_rejected_without_reordering() -> TestResult {
    let call_id = CallId::try_from(TestUuid::from_u128(42))?;
    let correlation = plakous_model::ToolCorrelation::new(
        Some("remote-exec".into()),
        None,
        ToolCallerLineage::Direct,
    )?;
    let result = plakous_model::ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TestTextBlock::new("done", None)?)],
        correlation.clone(),
    )?;
    let update = ToolResultUpdateBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        TestTextBlock::new("late", None)?,
        correlation.clone(),
    )?;
    let history = ConversationView::from_items(vec![ConversationItem::new(
        TestEntryId::try_from(TestUuid::from_u128(43))?,
        Actor::Tool,
        vec![
            ContentBlock::ToolResult(result),
            ContentBlock::ToolResultUpdate(update),
        ],
        Provenance::ToolExecution,
    )?])?;
    let mut pending = vec![PendingTool {
        call_id,
        remote_id: "remote-exec".into(),
        owner: ToolExecutionOwner::Local,
        correlation,
    }];
    let mut index = 0;
    let Err(error) = translate_tool_messages(history.items(), &mut index, &mut pending) else {
        return Err("post-terminal update was reordered instead of rejected".into());
    };
    assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
    assert!(error.message().contains("followed its terminal result"));
    Ok(())
}

#[test]
fn updates_and_media_fold_into_one_portable_terminal_result() -> TestResult {
    let call_id = CallId::try_from(uuid::Uuid::from_u128(42))?;
    let correlation = plakous_model::ToolCorrelation::new(
        Some("remote-exec".into()),
        None,
        ToolCallerLineage::Direct,
    )?;
    let update = ToolResultUpdateBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        plakous_model::TextBlock::new("working", None)?,
        correlation.clone(),
    )?;
    let result = plakous_model::ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![
            ToolResultPart::Image(plakous_model::ImageBlock::with_detail(
                "image/png",
                plakous_model::MediaSource::data_url("data:image/png;base64,AAAA")?,
                Some(plakous_model::ImageDetailHint::High),
            )?),
            ToolResultPart::Audio(plakous_model::AudioBlock::new(
                "audio/wav",
                plakous_model::MediaSource::data_url("data:audio/wav;base64,BBBB")?,
            )?),
        ],
        correlation.clone(),
    )?;
    let block = translate_tool_result(
        &result,
        &PendingTool {
            call_id,
            remote_id: "remote-exec".into(),
            owner: ToolExecutionOwner::Local,
            correlation,
        },
        &[&update],
    )?;
    let value = serde_json::to_value(block)?;
    assert_eq!(value["tool_use_id"], "remote-exec");
    assert_eq!(value["content"][0]["text"], "working");
    assert_eq!(
        value["content"][1],
        serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}
        })
    );
    assert_eq!(value["content"][2]["text"], "[audio result: audio/wav]");
    Ok(())
}

#[test]
fn inspect_image_bytes_reach_the_body_as_a_tool_result_image_block() -> TestResult {
    let call_id = CallId::try_from(uuid::Uuid::from_u128(44))?;
    let correlation = plakous_model::ToolCorrelation::new(
        Some("remote-inspect".into()),
        None,
        ToolCallerLineage::Direct,
    )?;
    let result = plakous_model::ToolResultBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![
            ToolResultPart::Text(plakous_model::TextBlock::new("media_type=image/png", None)?),
            ToolResultPart::Image(plakous_model::ImageBlock::new(
                "image/png",
                plakous_model::MediaSource::bytes(b"\x89PNG".to_vec())?,
            )?),
        ],
        correlation.clone(),
    )?;
    let block = translate_tool_result(
        &result,
        &PendingTool {
            call_id,
            remote_id: "remote-inspect".into(),
            owner: ToolExecutionOwner::Local,
            correlation,
        },
        &[],
    )?;
    let value = serde_json::to_value(block)?;
    assert_eq!(value["type"], "tool_result");
    assert_eq!(
        value["content"][1],
        serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw=="}
        })
    );
    assert!(!value.to_string().contains("[image result"));
    Ok(())
}

#[test]
fn user_images_lower_to_base64_image_blocks_in_order() -> TestResult {
    let blocks = vec![
        ContentBlock::Text(plakous_model::TextBlock::new("what is this", None)?),
        ContentBlock::Image(plakous_model::ImageBlock::new(
            "image/png",
            plakous_model::MediaSource::bytes(b"\x89PNG".to_vec())?,
        )?),
    ];
    let value = serde_json::to_value(translate_user(&blocks)?)?;
    assert_eq!(
        value,
        serde_json::json!([
            {"type": "text", "text": "what is this"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw=="}}
        ])
    );
    Ok(())
}

fn batch_call(n: u128) -> TestResult<ContentBlock> {
    Ok(ContentBlock::ToolCall(ToolCallBlock::new(
        CallId::try_from(TestUuid::from_u128(n))?,
        ToolName::new("read_file")?,
        ToolExecutionOwner::Local,
        plakous_model::ToolCallState::Requested,
        plakous_model::ToolCorrelation::new(
            Some(format!("call_{n}")),
            None,
            ToolCallerLineage::Direct,
        )?,
        "{}",
    )?))
}

fn batch_result(n: u128) -> TestResult<ContentBlock> {
    Ok(ContentBlock::ToolResult(
        plakous_model::ToolResultBlock::new(
            CallId::try_from(TestUuid::from_u128(n))?,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TestTextBlock::new("ok", None)?)],
            plakous_model::ToolCorrelation::new(
                Some(format!("call_{n}")),
                None,
                ToolCallerLineage::Direct,
            )?,
        )?,
    ))
}

fn batch_item(id: u128, actor: Actor, blocks: Vec<ContentBlock>) -> TestResult<ConversationItem> {
    let provenance = if actor == Actor::Tool {
        Provenance::ToolExecution
    } else {
        Provenance::ModelOutput
    };
    Ok(ConversationItem::new(
        TestEntryId::try_from(TestUuid::from_u128(id))?,
        actor,
        blocks,
        provenance,
    )?)
}

fn lower(items: Vec<ConversationItem>) -> Result<Vec<MessageParam>, MessagesRequestError> {
    let policy = policy().map_err(|_| invalid("test policy"))?;
    translate_history(
        &ConversationView::from_items(items).map_err(|_| invalid("test conversation"))?,
        &policy,
        &ToolOwnership::default(),
        false,
        true,
        &mut Vec::new(),
    )
    .map(|(messages, _, _)| messages)
}

#[test]
fn a_run_of_tool_call_items_folds_into_one_assistant_message() -> TestResult {
    let messages = lower(vec![
        batch_item(100, Actor::Assistant, vec![batch_call(1)?])?,
        batch_item(101, Actor::Assistant, vec![batch_call(2)?])?,
        batch_item(102, Actor::Tool, vec![batch_result(1)?, batch_result(2)?])?,
    ])?;
    let value = serde_json::to_value(&messages)?;
    assert_eq!(value.as_array().map(Vec::len), Some(2), "{value}");
    assert_eq!(value[0]["role"], "assistant");
    assert_eq!(value[0]["content"][0]["type"], "tool_use");
    assert_eq!(value[0]["content"][0]["id"], "call_1");
    assert_eq!(value[0]["content"][1]["type"], "tool_use");
    assert_eq!(value[0]["content"][1]["id"], "call_2");
    assert_eq!(value[1]["role"], "user");
    assert_eq!(value[1]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(value[1]["content"][1]["tool_use_id"], "call_2");
    Ok(())
}

#[test]
fn thinking_or_text_inside_an_open_tool_batch_is_refused() -> TestResult {
    let policy = policy()?;
    let replay = bind_replay_payload(
        &policy,
        ModelId::new("claude-exact")?,
        SIGNATURE_REPLAY_KIND,
        raw("\"opaque\"")?,
    )?;
    let thinking = ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
        ReasoningVisibility::Omitted,
        None,
        Some(replay),
    )?);
    let text = ContentBlock::Text(TestTextBlock::new("more", None)?);
    for intervening in [vec![thinking, batch_call(2)?], vec![text, batch_call(2)?]] {
        let Err(error) = lower(vec![
            batch_item(100, Actor::Assistant, vec![batch_call(1)?])?,
            batch_item(101, Actor::Assistant, intervening)?,
            batch_item(102, Actor::Tool, vec![batch_result(1)?, batch_result(2)?])?,
        ]) else {
            return Err("a block that is not tool_use folded behind an open tool_use".into());
        };
        assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
        assert!(
            error.message().contains("intervened"),
            "{}",
            error.message()
        );
    }
    Ok(())
}
