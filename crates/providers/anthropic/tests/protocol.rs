//! Thin Anthropic hook conformance over the Messages family kernel.

use std::collections::BTreeMap;

use plakous_anthropic::{
    AnthropicAccountIdentity, AnthropicApiKey, AnthropicErrorKind, AnthropicMessagesDialect,
    AnthropicMessagesStreamDecoder, AnthropicModel, AnthropicPlatformRoute, CLAUDE_OAUTH_IDENTITY,
    CLAUDE_OAUTH_MESSAGES_USER_AGENT, CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION,
    CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID, ClaudeProfile, MessagesRequestTranslator,
    ResolvedAnthropicMessages, ThinkingSupport, ToolOwnership,
};
use plakous_model::{
    Actor, AttemptId, BlockStart, CachePolicy, Capability, CapabilityConstraints,
    CapabilityRequirement, CapabilitySet, ConfiguredTool, ContentBlock, ConversationItem,
    ConversationView, Digest, DigestDomain, EffectiveProviderConfigurationId, EntryId,
    FactProvenance, FactSource, FeatureSupport, HostedTool, InstructionBlock, InstructionSource,
    ModelAdapterProfileId, ModelEvent, ModelFamilyId, ModelId, ModelRequest, ProtocolId,
    Provenance, ReasoningEffort, ReasoningPolicy, RequestMetadata, ResolvedModelDescriptor,
    RouteId, RunId, SelectedProviderConfigurationId, Support, SystemClock, SystemIdGenerator,
    TextBlock, Timestamp, ToolCallState, ToolCallerDisposition, ToolCallerLineage, ToolChoice,
    ToolConcurrency, ToolDefinition, ToolEffect, ToolExecutionDisposition, ToolExecutionOwner,
    ToolInputContract, ToolName, ToolOutputContract, TurnId, ValidationMode,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn resolved() -> TestResult<ResolvedAnthropicMessages> {
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let support = FeatureSupport::new(
        Support::Native,
        CapabilityConstraints::new(None, Some("offline fixture".to_owned())),
        provenance,
    );
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.platform")?,
        ModelId::new("claude-fixture")?,
        None,
        Some(ModelFamilyId::new("claude-fixture")?),
        ModelAdapterProfileId::new("claude.fixture.v1")?,
        CapabilitySet::new(
            BTreeMap::from([
                (Capability::Streaming, support.clone()),
                (Capability::Caching, support),
            ]),
            BTreeMap::new(),
        ),
    );
    ResolvedAnthropicMessages::new(
        AnthropicMessagesDialect::stable(),
        AnthropicPlatformRoute::for_account(
            AnthropicApiKey::new("sk-ant-offline-fixture")?,
            AnthropicAccountIdentity::new("offline-fixture-account")?,
        )?,
        AnthropicModel::new(
            ModelId::new("claude-fixture")?,
            ModelFamilyId::new("claude-fixture")?,
            1024,
            ThinkingSupport::Disabled,
        )?,
        ClaudeProfile::new(ModelAdapterProfileId::new("claude.fixture.v1")?),
        descriptor,
    )
    .map_err(Into::into)
}

fn hosted_resolved() -> TestResult<ResolvedAnthropicMessages> {
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let native = |label: &str| {
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::new(None, Some(label.to_owned())),
            provenance,
        )
    };
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.platform")?,
        ModelId::new("claude-sonnet-5")?,
        None,
        Some(ModelFamilyId::new("claude-5-sonnet")?),
        ModelAdapterProfileId::new(CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID)?,
        CapabilitySet::new(
            BTreeMap::from([
                (Capability::Streaming, native("streaming")),
                (Capability::Tools, native("tools")),
                (Capability::HostedProgrammaticToolCalling, native("ptc")),
            ]),
            BTreeMap::new(),
        ),
    );
    ResolvedAnthropicMessages::new(
        AnthropicMessagesDialect::stable(),
        AnthropicPlatformRoute::for_account(
            AnthropicApiKey::new("sk-ant-hosted-offline")?,
            AnthropicAccountIdentity::new("hosted-offline-account")?,
        )?,
        AnthropicModel::new(
            ModelId::new("claude-sonnet-5")?,
            ModelFamilyId::new("claude-5-sonnet")?,
            8_192,
            ThinkingSupport::Disabled,
        )?,
        ClaudeProfile::new(ModelAdapterProfileId::new(
            CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID,
        )?),
        descriptor,
    )
    .map_err(Into::into)
}

fn hosted_prepared() -> TestResult<plakous_anthropic::PreparedMessagesRequest> {
    hosted_prepared_with_ownership(ToolOwnership::default())
}

fn hosted_prepared_with_ownership(
    ownership: ToolOwnership,
) -> TestResult<plakous_anthropic::PreparedMessagesRequest> {
    tool_prepared(
        ToolCallerDisposition::DirectAndProviderProgrammatic,
        true,
        ownership,
    )
}

fn tool_prepared(
    caller: ToolCallerDisposition,
    programmatic: bool,
    ownership: ToolOwnership,
) -> TestResult<plakous_anthropic::PreparedMessagesRequest> {
    let resolved = hosted_resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"hosted-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"hosted-effective"],
        )?)?)?;
    let schema = serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false});
    let tool = ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("weather")?,
            "1.0.0",
            "Gets weather",
            ToolInputContract::structured(schema)?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?,
        ToolExecutionDisposition::Local,
        caller,
    );
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(101))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "Use Python to check Paris weather",
                None,
            )?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(102))?,
            TurnId::try_from(Uuid::from_u128(103))?,
            AttemptId::try_from(Uuid::from_u128(104))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .tools(vec![tool]);
    let request = if programmatic {
        request
            .hosted_tools(vec![HostedTool::ProgrammaticComposition])
            .build()?
    } else {
        request.build()?
    };
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    MessagesRequestTranslator::new(resolved)
        .translate(&plan, ownership)
        .map_err(Into::into)
}

fn direct_prepared(
    resolved: ResolvedAnthropicMessages,
    seed: &'static [u8],
) -> TestResult<plakous_anthropic::PreparedMessagesRequest> {
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[seed, b"selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[seed, b"effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(111))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(112))?,
            TurnId::try_from(Uuid::from_u128(113))?,
            AttemptId::try_from(Uuid::from_u128(114))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?))
        .translate(&plan, ToolOwnership::default())
        .map_err(Into::into)
}

fn assert_hosted_surfaces_rejected(
    prepared: plakous_anthropic::PreparedMessagesRequest,
    reported_model: &str,
) -> TestResult {
    let start = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_direct\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"{reported_model}\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n"
    );
    let surfaces = [
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"code_execution\",\"input\":{\"code\":\"print(1)\"}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"code_execution_tool_result\",\"tool_use_id\":\"srv_1\",\"content\":{\"type\":\"code_execution_result\",\"stdout\":\"safe fixture\",\"stderr\":\"\",\"return_code\":0,\"content\":[]}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"weather\",\"input\":{},\"caller\":{\"type\":\"code_execution_20260120\",\"tool_id\":\"srv_1\"}}}\n\n",
    ];
    for surface in surfaces {
        let mut decoder = AnthropicMessagesStreamDecoder::new(
            prepared.clone(),
            std::sync::Arc::new(SystemClock),
            std::sync::Arc::new(SystemIdGenerator),
        )?;
        decoder.push(start.as_bytes())?;
        let Err(error) = decoder.push(surface.as_bytes()) else {
            return Err("hosted surface projected for a direct-only request".into());
        };
        assert_eq!(error.kind(), AnthropicErrorKind::InvalidEvent);
    }
    let with_container = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-08-23T12:00:00Z\"},\"stop_reason\":null,\"stop_sequence\":null},\"usage\":{\"output_tokens\":0}}\n\n";
    let mut decoder = AnthropicMessagesStreamDecoder::new(
        prepared,
        std::sync::Arc::new(SystemClock),
        std::sync::Arc::new(SystemIdGenerator),
    )?;
    decoder.push(start.as_bytes())?;
    let Err(error) = decoder.push(with_container.as_bytes()) else {
        return Err("container projected for a direct-only request".into());
    };
    assert_eq!(error.kind(), AnthropicErrorKind::InvalidEvent);
    Ok(())
}

#[test]
fn hosted_ptc_request_is_exact_and_customer_tool_allows_both_callers() -> TestResult {
    let prepared = hosted_prepared()?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(body["tools"][0]["name"], "weather");
    assert_eq!(
        body["tools"][0]["allowed_callers"],
        serde_json::json!([
            "direct",
            "code_execution_20260120",
            "code_execution_20260521"
        ])
    );
    assert_eq!(
        body["tools"][1],
        serde_json::json!({"type":"code_execution_20260120","name":"code_execution"})
    );
    assert!(body.get("container").is_none());
    Ok(())
}

#[test]
fn hosted_ptc_retains_mcp_owner_and_programmatic_wire_shape() -> TestResult {
    let mut owners = BTreeMap::new();
    let mcp = ToolExecutionOwner::Mcp {
        server: plakous_model::McpServerId::new("weather-mcp")?,
    };
    owners.insert(ToolName::new("weather")?, mcp.clone());
    let prepared = hosted_prepared_with_ownership(ToolOwnership::new(owners))?;
    assert_eq!(
        prepared.tool_ownership().owner(&ToolName::new("weather")?),
        mcp
    );
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(
        body["tools"][0]["allowed_callers"],
        serde_json::json!([
            "direct",
            "code_execution_20260120",
            "code_execution_20260521"
        ])
    );
    Ok(())
}

#[test]
fn direct_mcp_batch_keeps_exact_owner_and_wire_shape() -> TestResult {
    let mut owners = BTreeMap::new();
    let mcp = ToolExecutionOwner::Mcp {
        server: plakous_model::McpServerId::new("weather-mcp")?,
    };
    owners.insert(ToolName::new("weather")?, mcp.clone());
    let prepared = tool_prepared(
        ToolCallerDisposition::DirectOnly,
        false,
        ToolOwnership::new(owners),
    )?;
    assert_eq!(
        prepared.tool_ownership().owner(&ToolName::new("weather")?),
        mcp
    );
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(body["tools"][0]["name"], "weather");
    assert!(body["tools"][0].get("allowed_callers").is_none());
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    Ok(())
}

#[test]
fn programmatic_customer_call_retains_local_owner_and_lineage() -> TestResult {
    let mut decoder = AnthropicMessagesStreamDecoder::new(
        hosted_prepared()?,
        std::sync::Arc::new(SystemClock),
        std::sync::Arc::new(SystemIdGenerator),
    )?;
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ptc\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"code_execution\",\"input\":{\"code\":\"weather(city='Paris')\"}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"weather\",\"input\":{\"city\":\"Paris\"},\"caller\":{\"type\":\"code_execution_20260120\",\"tool_id\":\"srv_1\"}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-08-23T12:00:00Z\"},\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let events = decoder.push(sse.as_bytes())?;
    let nested = events
        .iter()
        .find_map(|event| match event {
            ModelEvent::BlockStarted(start) => match start.block() {
                BlockStart::ToolCall {
                    tool,
                    owner,
                    state,
                    correlation,
                    ..
                } if tool.as_str() == "weather" => Some((owner, state, correlation)),
                _ => None,
            },
            _ => None,
        })
        .ok_or("nested weather call")?;
    assert_eq!(nested.0, &ToolExecutionOwner::Local);
    assert_eq!(nested.1, &ToolCallState::Requested);
    assert_eq!(nested.2.container_id(), Some("container_1"));
    assert_eq!(
        nested
            .2
            .caller_lineage()
            .provider_programmatic_execution_id(),
        Some("srv_1")
    );
    assert!(matches!(
        nested.2.caller_lineage(),
        ToolCallerLineage::ProviderProgrammatic { .. }
    ));
    let candidate = events
        .iter()
        .find_map(|event| match event {
            ModelEvent::ResponseCompleted(response) => response.family_artifact_candidate(),
            _ => None,
        })
        .ok_or("hosted Messages family artifact candidate")?;
    assert_eq!(candidate.family(), "messages");
    assert_eq!(candidate.schema_revision(), 1);
    assert_eq!(candidate.codec_revision(), 1);
    assert!(candidate.bytes().starts_with(b"PLAKMSG\0"));
    assert!(
        !candidate
            .bytes()
            .windows(10)
            .any(|part| part == b"expires_at")
    );
    Ok(())
}

#[test]
fn programmatic_customer_call_retains_mcp_owner_and_lineage() -> TestResult {
    let mcp = ToolExecutionOwner::Mcp {
        server: plakous_model::McpServerId::new("weather-mcp")?,
    };
    let mut decoder = AnthropicMessagesStreamDecoder::new(
        hosted_prepared_with_ownership(ToolOwnership::new(BTreeMap::from([(
            ToolName::new("weather")?,
            mcp.clone(),
        )])))?,
        std::sync::Arc::new(SystemClock),
        std::sync::Arc::new(SystemIdGenerator),
    )?;
    let events = decoder.push(concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ptc_mcp\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_mcp\",\"name\":\"code_execution\",\"input\":{\"code\":\"weather(city='Paris')\"}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_mcp\",\"name\":\"weather\",\"input\":{\"city\":\"Paris\"},\"caller\":{\"type\":\"code_execution_20260120\",\"tool_id\":\"srv_mcp\"}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_mcp\",\"expires_at\":\"2026-08-23T12:00:00Z\"},\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    ).as_bytes())?;
    let (owner, lineage) = events
        .iter()
        .find_map(|event| match event {
            ModelEvent::BlockStarted(start) => match start.block() {
                BlockStart::ToolCall {
                    tool,
                    owner,
                    correlation,
                    ..
                } if tool.as_str() == "weather" => Some((owner, correlation.caller_lineage())),
                _ => None,
            },
            _ => None,
        })
        .ok_or("nested MCP weather call")?;
    assert_eq!(owner, &mcp);
    assert_eq!(
        lineage.provider_programmatic_execution_id(),
        Some("srv_mcp")
    );
    Ok(())
}

#[test]
fn hosted_terminal_end_turn_seals_native_artifact_without_requesting_continuation() -> TestResult {
    let mut decoder = AnthropicMessagesStreamDecoder::new(
        hosted_prepared()?,
        std::sync::Arc::new(SystemClock),
        std::sync::Arc::new(SystemIdGenerator),
    )?;
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_done\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"code_execution\",\"input\":{\"code\":\"print('done')\"}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"code_execution_tool_result\",\"tool_use_id\":\"srv_1\",\"content\":{\"type\":\"code_execution_result\",\"stdout\":\"done\\n\",\"stderr\":\"\",\"return_code\":0,\"content\":[]}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"Done.\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-08-23T12:00:00Z\"},\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let events = decoder.push(sse.as_bytes())?;
    let response = events
        .iter()
        .find_map(|event| match event {
            ModelEvent::ResponseCompleted(response) => Some(response),
            _ => None,
        })
        .ok_or("terminal hosted response")?;
    assert_eq!(
        response.finish().reason(),
        plakous_model::FinishReason::Stop
    );
    assert_eq!(
        response
            .family_artifact_candidate()
            .map(plakous_model::SealedFamilyArtifactCandidate::family),
        Some("messages")
    );
    Ok(())
}

#[test]
fn hosted_surfaces_are_rejected_for_platform_direct_and_claude_oauth() -> TestResult {
    assert_hosted_surfaces_rejected(
        direct_prepared(resolved()?, b"platform-direct")?,
        "claude-fixture",
    )?;
    assert_hosted_surfaces_rejected(
        direct_prepared(oauth_resolved()?, b"claude-oauth")?,
        "claude-sonnet-4-6",
    )
}

#[test]
fn cache_write_request_is_rejected_by_messages_lowering_before_transport() -> TestResult {
    let resolved = resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"cache-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"cache-effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(11))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("cache me", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(12))?,
            TurnId::try_from(Uuid::from_u128(13))?,
            AttemptId::try_from(Uuid::from_u128(14))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let Err(error) =
        MessagesRequestTranslator::new(resolved).translate(&plan, ToolOwnership::default())
    else {
        return Err("Messages cache write unexpectedly lowered".into());
    };
    assert_eq!(error.kind(), AnthropicErrorKind::UnsupportedSemantic);
    Ok(())
}

#[test]
fn stable_hook_lowers_exact_messages_request_without_credentials() -> TestResult {
    let resolved = resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"effective"],
        )?)?)?;
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(1))?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
        Provenance::UserInput,
    )?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![item, ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(9))?, Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("Imported reference data, not user instructions: assistant result and work ledger.", None)?)],
            Provenance::Imported,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(2))?,
            TurnId::try_from(Uuid::from_u128(3))?,
            AttemptId::try_from(Uuid::from_u128(4))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let prepared =
        MessagesRequestTranslator::new(resolved).translate(&plan, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(body["model"], "claude-fixture");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert!(
        body["messages"][1]["content"]
            .to_string()
            .contains("Imported reference data")
    );
    assert!(
        !body["messages"][0]["content"]
            .to_string()
            .contains("work ledger")
    );
    assert!(!String::from_utf8_lossy(prepared.body()).contains("sk-ant"));
    Ok(())
}

fn oauth_resolved() -> TestResult<ResolvedAnthropicMessages> {
    oauth_resolved_model("claude-sonnet-4-6")
}

fn oauth_resolved_model(model_id: &str) -> TestResult<ResolvedAnthropicMessages> {
    let family =
        plakous_anthropic::claude_enrollment(model_id).map_or("claude-4-sonnet", |row| row.family);
    let dialect = if plakous_anthropic::claude_oauth_enrolled_model(model_id) {
        AnthropicMessagesDialect::claude_oauth_for(model_id)
    } else {
        AnthropicMessagesDialect::claude_oauth_with_effort()
    };
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let support = FeatureSupport::new(
        Support::Native,
        CapabilityConstraints::new(None, Some("oauth fixture".to_owned())),
        provenance,
    );
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.claude_oauth")?,
        ModelId::new(model_id)?,
        None,
        Some(ModelFamilyId::new(family)?),
        ModelAdapterProfileId::new("anthropic.claude.oauth.compat.2026-09-22.r2")?,
        CapabilitySet::new(
            BTreeMap::from([
                (Capability::Streaming, support.clone()),
                (Capability::Tools, support.clone()),
                (Capability::Caching, support.clone()),
                (Capability::Reasoning, support),
            ]),
            BTreeMap::new(),
        ),
    );
    ResolvedAnthropicMessages::new(
        dialect,
        AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-fixture")?,
            AnthropicAccountIdentity::new("oauth-fixture-account")?,
        )?,
        AnthropicModel::new(
            ModelId::new(model_id)?,
            ModelFamilyId::new(family)?,
            128_000,
            ThinkingSupport::Adaptive,
        )?,
        ClaudeProfile::new(ModelAdapterProfileId::new(
            "anthropic.claude.oauth.compat.2026-09-22.r2",
        )?),
        descriptor,
    )
    .map_err(Into::into)
}

fn assert_oauth_billing_attested(
    raw: &[u8],
    system0: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = system0["text"].as_str().ok_or("billing text")?;
    assert!(
        text.starts_with("x-anthropic-billing-header: cc_version=2.1.295."),
        "{text}"
    );
    assert!(text.contains("; cc_entrypoint=cli; cch="), "{text}");
    assert!(!text.contains("cch=00000"), "{text}");
    assert!(
        plakous_anthropic::claude_oauth_cch_matches_body(raw),
        "cch does not match the native byte-range hash of the wire body"
    );
    Ok(())
}

#[test]
fn claude_oauth_required_cache_is_admitted_and_emitted() -> TestResult {
    let resolved = oauth_resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"oauth-cache-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"oauth-cache-effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(31))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("cache me", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(32))?,
            TurnId::try_from(Uuid::from_u128(33))?,
            AttemptId::try_from(Uuid::from_u128(34))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::Profile,
        "Stable cached instruction",
    )?])
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let prepared = MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?))
        .translate(&plan, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let system = body["system"].as_array().ok_or("system blocks")?;
    assert_oauth_billing_attested(prepared.body(), &system[0])?;
    assert!(system[0].get("cache_control").is_none());
    assert_eq!(system[1]["text"], CLAUDE_OAUTH_IDENTITY);
    assert!(system[1].get("cache_control").is_none());
    assert_eq!(
        system[2]["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"})
    );
    assert!(body.get("cache_control").is_none());
    let messages = body["messages"].as_array().ok_or("messages")?;
    let last = messages.last().ok_or("last message")?;
    assert_eq!(last["role"], "user");
    let last_block = last["content"]
        .as_array()
        .and_then(|blocks| blocks.last())
        .ok_or("last content")?;
    assert_eq!(
        last_block["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"})
    );
    Ok(())
}

#[test]
fn claude_oauth_rolling_last_message_marks_only_the_latest_text_block() -> TestResult {
    let resolved = oauth_resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"oauth-cache-rolling-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"oauth-cache-rolling-effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(41))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("first", None)?)],
                Provenance::UserInput,
            )?,
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(42))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("second", None)?)],
                Provenance::UserInput,
            )?,
        ])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(43))?,
            TurnId::try_from(Uuid::from_u128(44))?,
            AttemptId::try_from(Uuid::from_u128(45))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::Profile,
        "Stable cached instruction",
    )?])
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let prepared = MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?))
        .translate(&plan, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let system = body["system"].as_array().ok_or("system blocks")?;
    assert_oauth_billing_attested(prepared.body(), &system[0])?;
    assert!(system[0].get("cache_control").is_none());
    assert_eq!(system[1]["text"], CLAUDE_OAUTH_IDENTITY);
    assert!(system[1].get("cache_control").is_none());
    assert_eq!(
        system[2]["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"})
    );
    let messages = body["messages"].as_array().ok_or("messages")?;
    assert_eq!(messages.len(), 1);
    let content = messages[0]["content"].as_array().ok_or("merged content")?;
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["text"], "first");
    assert!(content[0].get("cache_control").is_none());
    assert_eq!(content[1]["text"], "second");
    assert_eq!(
        content[1]["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"})
    );
    Ok(())
}

#[test]
fn claude_oauth_last_customer_tool_emits_one_hour_cache_control() -> TestResult {
    let resolved = oauth_resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"oauth-cache-tool-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"oauth-cache-tool-effective"],
        )?)?)?;
    let tool = ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            "Look up one value",
            ToolInputContract::structured(serde_json::json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"q":{"type":"string"}},
                "required":["q"],
                "additionalProperties":false
            }))?,
            ToolOutputContract::portable_model_content(),
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
            EntryId::try_from(Uuid::from_u128(51))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("use lookup", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(52))?,
            TurnId::try_from(Uuid::from_u128(53))?,
            AttemptId::try_from(Uuid::from_u128(54))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .tools(vec![tool])
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let prepared = MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?))
        .translate(&plan, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(
        body["tools"][0]["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"})
    );
    Ok(())
}

#[test]
fn claude_oauth_lowering_uses_gateway_prefix_then_projection() -> TestResult {
    let resolved = oauth_resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"oauth-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"oauth-effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(21))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(22))?,
            TurnId::try_from(Uuid::from_u128(23))?,
            AttemptId::try_from(Uuid::from_u128(24))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::Profile,
        "Use Bash, Read, Edit, Write, and TaskStop.",
    )?])
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    assert!(
        MessagesRequestTranslator::new(resolved.clone())
            .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
                99
            ))?))
            .translate(&plan, ToolOwnership::default())
            .is_err()
    );
    let prepared = MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?))
        .translate(&plan, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let system = body["system"].as_array().ok_or("system blocks")?;
    assert_oauth_billing_attested(prepared.body(), &system[0])?;
    assert!(system[0].get("cache_control").is_none());
    assert_eq!(system[1]["text"], CLAUDE_OAUTH_IDENTITY);
    assert!(system[1].get("cache_control").is_none());
    assert_eq!(
        system[2]["text"],
        "Use Bash, Read, Edit, Write, and TaskStop."
    );
    assert_eq!(
        system[2]["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"})
    );
    assert!(body.get("cache_control").is_none());
    let last_block = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .and_then(|blocks| blocks.last())
        .ok_or("last user content")?;
    assert_eq!(
        last_block["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"})
    );
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
    let user_id = body["metadata"]["user_id"]
        .as_str()
        .ok_or("metadata user_id")?;
    let metadata: serde_json::Value = serde_json::from_str(user_id)?;
    assert!(
        metadata["device_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(metadata["device_id"], "ab".repeat(32));
    assert_eq!(
        metadata["account_uuid"],
        "12345678-1234-4234-8234-123456789abc"
    );
    assert!(
        metadata["session_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    Ok(())
}

#[test]
fn claude_oauth_effort_projects_adaptive_thinking_and_exact_output_config() -> TestResult {
    let resolved = oauth_resolved()?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"oauth-effort-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"oauth-effort-effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(31))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("reason", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(32))?,
            TurnId::try_from(Uuid::from_u128(33))?,
            AttemptId::try_from(Uuid::from_u128(34))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::XHigh),
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let prepared = MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?))
        .translate(&plan, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(
        body["thinking"],
        serde_json::json!({"type":"adaptive","display":"updates"})
    );
    assert_eq!(body["output_config"], serde_json::json!({"effort":"xhigh"}));
    assert!(
        prepared
            .dialect()
            .beta_header()
            .as_deref()
            .is_some_and(|betas| betas
                .split(',')
                .any(|beta| beta == "thinking-display-updates-2026-08-18"))
    );
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;
    assert!(body.get("metadata").is_some());
    Ok(())
}

#[test]
fn opus_5_5_keeps_adaptive_thinking_for_tool_free_compaction_and_rejects_disabled() -> TestResult {
    let resolved = oauth_resolved_model("claude-opus-5-5")?;
    let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"opus-5-5-selected"],
    )?)?)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"opus-5-5-effective"],
    )?)?)?;
    let build = |reasoning| -> TestResult<ModelRequest> {
        ModelRequest::builder(
            selected,
            effective,
            ConversationView::from_items(vec![ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(401))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("summarize", None)?)],
                Provenance::UserInput,
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(402))?,
                TurnId::try_from(Uuid::from_u128(403))?,
                AttemptId::try_from(Uuid::from_u128(404))?,
                Timestamp::new(2, 0)?,
            ),
        )
        .tool_choice(ToolChoice::None)
        .reasoning(reasoning)
        .build()
        .map_err(Into::into)
    };
    let adaptive = build(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Low),
        requirement: CapabilityRequirement::Required,
    })?;
    let adaptive = resolved
        .descriptor()
        .validate_request(adaptive, ValidationMode::Strict)?;
    let translator = MessagesRequestTranslator::new(resolved.clone())
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?));
    let prepared = translator.translate(&adaptive, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(
        body["thinking"],
        serde_json::json!({"type":"adaptive","display":"updates"})
    );
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("tools").is_none());
    let disabled = resolved
        .descriptor()
        .validate_request(build(ReasoningPolicy::Disabled)?, ValidationMode::Strict)?;
    let rejected = translator
        .translate(&disabled, ToolOwnership::default())
        .err()
        .ok_or("disabled thinking must fail")?;
    assert_eq!(rejected.kind(), AnthropicErrorKind::InvalidRequest);
    Ok(())
}

#[test]
fn sonnet_5_5_admits_adaptive_auto_tools_but_rejects_disabled_and_forced_tools() -> TestResult {
    let resolved = oauth_resolved_model("claude-sonnet-5-5")?;
    let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"sonnet-5-5-selected"],
    )?)?)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"sonnet-5-5-effective"],
    )?)?)?;
    let tool = ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            "Look up a value",
            ToolInputContract::structured(serde_json::json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object", "properties":{}, "required":[], "additionalProperties":false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    );
    let build = |reasoning, choice| -> TestResult<ModelRequest> {
        Ok(ModelRequest::builder(
            selected,
            effective,
            ConversationView::from_items(vec![ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(411))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("look up a value", None)?)],
                Provenance::UserInput,
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(412))?,
                TurnId::try_from(Uuid::from_u128(413))?,
                AttemptId::try_from(Uuid::from_u128(414))?,
                Timestamp::new(2, 0)?,
            ),
        )
        .tools(vec![tool.clone()])
        .tool_choice(choice)
        .reasoning(reasoning)
        .build()?)
    };
    let adaptive = ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    };
    let translator = MessagesRequestTranslator::new(resolved.clone())
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?));
    let auto = resolved
        .descriptor()
        .validate_request(build(adaptive, ToolChoice::Auto)?, ValidationMode::Strict)?;
    let prepared = translator.translate(&auto, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(
        body["thinking"],
        serde_json::json!({"type":"adaptive","display":"updates"})
    );
    assert_eq!(
        body["output_config"],
        serde_json::json!({"effort":"medium"})
    );

    for (reasoning, choice) in [
        (ReasoningPolicy::Disabled, ToolChoice::Auto),
        (adaptive, ToolChoice::Required),
        (adaptive, ToolChoice::Specific(ToolName::new("lookup")?)),
    ] {
        let plan = resolved
            .descriptor()
            .validate_request(build(reasoning, choice)?, ValidationMode::Strict)?;
        let rejected = translator
            .translate(&plan, ToolOwnership::default())
            .err()
            .ok_or("Sonnet 5.5 request must fail before dispatch")?;
        assert_eq!(rejected.kind(), AnthropicErrorKind::InvalidRequest);
    }
    Ok(())
}

fn haiku_oauth_plan(
    reasoning: ReasoningPolicy,
    choice: ToolChoice,
    salt: &[u8],
    with_tool: bool,
) -> TestResult<(
    ResolvedAnthropicMessages,
    plakous_model::ValidatedRequestPlan,
)> {
    costume_oauth_plan("claude-haiku-5-5", reasoning, choice, salt, with_tool)
}

fn costume_oauth_plan(
    model: &str,
    reasoning: ReasoningPolicy,
    choice: ToolChoice,
    salt: &[u8],
    with_tool: bool,
) -> TestResult<(
    ResolvedAnthropicMessages,
    plakous_model::ValidatedRequestPlan,
)> {
    let resolved = oauth_resolved_model(model)?;
    let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[salt],
    )?)?)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[salt],
    )?)?)?;
    let mut builder = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(421))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(422))?,
            TurnId::try_from(Uuid::from_u128(423))?,
            AttemptId::try_from(Uuid::from_u128(424))?,
            Timestamp::new(2, 0)?,
        ),
    );
    if with_tool {
        let tool = ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new("lookup")?,
                "1.0.0",
                "Look up a value",
                ToolInputContract::structured(serde_json::json!({
                    "$schema":"https://json-schema.org/draft/2020-12/schema",
                    "type":"object", "properties":{}, "required":[], "additionalProperties":false
                }))?,
                ToolOutputContract::portable_model_content(),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        );
        builder = builder.tools(vec![tool]);
    }
    let request = builder.tool_choice(choice).reasoning(reasoning).build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    Ok((resolved, plan))
}

/// Claude Code 2.1.295 `anthropic-beta` and Messages `max_tokens` are per model.
///
/// Every row was captured under 2.1.295 (low effort); headers and `max_tokens`
/// match the earlier rows. Sonnet 5 is the 14-beta
/// baseline; Opus 5 adds `mid-conversation-tool-changes`; Sonnet 5.5, Opus 5.5,
/// and Fable add that and `per-turn-control`; Haiku 5.5 sends the same 16 with
/// `claude-code-20250219` seventh. Fable's id is not Haiku, so it takes the
/// non-Haiku 16-beta header. Effort does not change the header.
/// An OAuth body sends `context_management` when thinking is enabled or
/// adaptive, and a first request sends `diagnostics` with a `null` previous id.
#[test]
fn claude_oauth_costume_is_per_enrolled_model() -> TestResult {
    const SONNET_5: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
    const OPUS_5: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,mid-conversation-tool-changes-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
    const BOTH: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
    const HAIKU: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,claude-code-20250219,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
    assert_eq!(
        CLAUDE_OAUTH_MESSAGES_USER_AGENT,
        "claude-cli/2.1.295 (external, cli)"
    );
    assert_eq!(CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION, "0.128.0");
    let rows = [
        ("claude-sonnet-5", 64_000_u32, SONNET_5),
        ("claude-opus-5", 64_000, OPUS_5),
        ("claude-sonnet-5-5", 128_000, BOTH),
        ("claude-opus-5-5", 128_000, BOTH),
        ("claude-haiku-5-5", 128_000, HAIKU),
        ("claude-fable-5-1", 64_000, BOTH),
    ];
    let context_management = serde_json::json!({
        "edits": [{"type": "clear_thinking_20251015", "keep": "all"}]
    });
    for (model, max_tokens, betas) in rows {
        assert_eq!(
            plakous_anthropic::claude_oauth_beta_header(model),
            betas,
            "{model}"
        );
        assert_eq!(
            plakous_anthropic::claude_oauth_request_max_tokens(model),
            Some(max_tokens),
            "{model}"
        );
        let (resolved, plan) = costume_oauth_plan(
            model,
            ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Low),
                requirement: CapabilityRequirement::Required,
            },
            ToolChoice::Auto,
            model.as_bytes(),
            false,
        )?;
        let prepared = MessagesRequestTranslator::new(resolved)
            .with_oauth_identity(fixture_oauth_identity()?)
            .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
                99,
            ))?))
            .translate(&plan, ToolOwnership::default())?;
        let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
        assert_eq!(body["model"], model);
        assert_eq!(body["max_tokens"].as_u64(), Some(u64::from(max_tokens)));
        assert_eq!(body["context_management"], context_management);
        assert_eq!(
            body["diagnostics"],
            serde_json::json!({"previous_message_id": null}),
            "{model}"
        );
        assert_eq!(
            prepared.dialect().beta_header().as_deref(),
            Some(betas),
            "{model}"
        );
    }
    let mut haiku_headers = Vec::new();
    for (effort, salt) in [
        (ReasoningEffort::Low, &b"haiku-5-5-low"[..]),
        (ReasoningEffort::Max, &b"haiku-5-5-max"[..]),
    ] {
        let (resolved, plan) = haiku_oauth_plan(
            ReasoningPolicy::Enabled {
                effort: Some(effort),
                requirement: CapabilityRequirement::Required,
            },
            ToolChoice::Auto,
            salt,
            false,
        )?;
        let prepared = MessagesRequestTranslator::new(resolved)
            .with_oauth_identity(fixture_oauth_identity()?)
            .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
                99,
            ))?))
            .translate(&plan, ToolOwnership::default())?;
        haiku_headers.push(prepared.dialect().beta_header());
        let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
        assert_eq!(body["max_tokens"].as_u64(), Some(128_000));
        assert_eq!(body["context_management"], context_management);
        assert_eq!(
            body["diagnostics"],
            serde_json::json!({"previous_message_id": null})
        );
    }
    assert_eq!(haiku_headers[0], haiku_headers[1]);
    assert_eq!(haiku_headers[0].as_deref(), Some(HAIKU));
    Ok(())
}

#[test]
fn haiku_5_5_rejects_disabled_thinking_and_admits_forced_tool_choice() -> TestResult {
    let (resolved, disabled) = haiku_oauth_plan(
        ReasoningPolicy::Disabled,
        ToolChoice::Auto,
        b"haiku-5-5-disabled",
        false,
    )?;
    let translator = MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?));
    let rejected = translator
        .translate(&disabled, ToolOwnership::default())
        .err()
        .ok_or("disabled thinking must fail before dispatch")?;
    assert_eq!(rejected.kind(), AnthropicErrorKind::InvalidRequest);
    assert_eq!(
        rejected.to_string(),
        "Claude 5.5 requires adaptive thinking"
    );

    let (resolved, forced) = haiku_oauth_plan(
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        },
        ToolChoice::Required,
        b"haiku-5-5-forced",
        true,
    )?;
    let prepared = MessagesRequestTranslator::new(resolved)
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?))
        .translate(&forced, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["tool_choice"], serde_json::json!({"type":"any"}));
    assert_eq!(
        body["thinking"],
        serde_json::json!({"type":"adaptive","display":"updates"})
    );
    Ok(())
}

fn billing_prompt(id: u128, text: &str, provenance: Provenance) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(id))?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None)?)],
        provenance,
    )?)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture walks the prompt, continuation and diagnostics attribution in order"
)]
fn oauth_billing_carries_truthful_prompt_and_previous_request_before_attestation() -> TestResult {
    let resolved = oauth_resolved()?;
    let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"lineage-selected"],
    )?)?)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"lineage-effective"],
    )?)?)?;
    let first = billing_prompt(101, "hello", Provenance::UserInput)?;
    let metadata = RequestMetadata::for_turn(
        RunId::try_from(Uuid::from_u128(102))?,
        TurnId::try_from(Uuid::from_u128(103))?,
        AttemptId::try_from(Uuid::from_u128(104))?,
        Timestamp::new(2, 0)?,
    );
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![first.clone()])?,
        metadata,
    )
    .build()?;
    let first_plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let translator = MessagesRequestTranslator::new(resolved.clone())
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?));
    let prepared = translator.translate(&first_plan, ToolOwnership::default())?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let header = body["system"][0]["text"].as_str().ok_or("billing")?;
    assert!(header.starts_with("x-anthropic-billing-header: cc_version=2.1.295.05c;"));
    assert!(
        header.ends_with(
            "; cc_prompt_id=00000000-0000-0000-0000-000000000065; cc_turn_origin=human;"
        )
    );
    assert!(!header.contains("cc_prev_req"));
    assert_eq!(
        body["diagnostics"],
        serde_json::json!({"previous_message_id": null})
    );
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;
    let next = billing_prompt(201, "a different later human prompt", Provenance::UserInput)?;
    let imported = billing_prompt(
        301,
        "imported metadata must not own a prompt",
        Provenance::Imported,
    )?;
    let request = ModelRequest::from_parts(
        first_plan.request().stable_prefix().clone(),
        ConversationView::from_items(vec![first, next, imported])?,
        metadata,
    )?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let previous = plakous_model::ModelResponseMetadata::new(
        None,
        Some(plakous_model::ModelProviderRequestId::new("req_prior-123")?),
    );
    let prepared = translator.translate_with_provenance(
        &plan,
        ToolOwnership::default(),
        None,
        Some(&previous),
    )?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let header = body["system"][0]["text"].as_str().ok_or("billing")?;
    assert!(header.starts_with("x-anthropic-billing-header: cc_version=2.1.295.05c;"));
    assert!(header.ends_with(
        "; cc_prev_req=req_prior-123; cc_prompt_id=00000000-0000-0000-0000-0000000000c9; cc_turn_origin=human;"
    ));
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;
    // Claude Code 2.1.295 captures: the indices follow `cc_turn_origin`, and
    // `diagnostics` names the response that `cc_prev_req` names.
    let continued = translator
        .clone()
        .with_continuation(Some("msg_nc005"), plakous_model::TurnPosition::new(2, 3));
    let prepared = continued.translate_with_provenance(
        &plan,
        ToolOwnership::default(),
        None,
        Some(&previous),
    )?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let header = body["system"][0]["text"].as_str().ok_or("billing")?;
    assert!(header.ends_with(
        "; cc_prev_req=req_prior-123; cc_prompt_id=00000000-0000-0000-0000-0000000000c9; cc_turn_origin=human; cc_prompt_index=2; cc_turn_index=3;"
    ));
    assert_eq!(
        body["diagnostics"],
        serde_json::json!({"previous_message_id": "msg_nc005"})
    );
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;
    let foreign = translator
        .clone()
        .with_continuation(Some("resp_not-a-message"), None);
    let prepared = foreign.translate_with_provenance(
        &plan,
        ToolOwnership::default(),
        None,
        Some(&previous),
    )?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert_eq!(
        body["diagnostics"],
        serde_json::json!({"previous_message_id": null})
    );
    let malformed = plakous_model::ModelResponseMetadata::new(
        None,
        Some(plakous_model::ModelProviderRequestId::new(
            "msg_wrong-kind",
        )?),
    );
    let prepared = translator.translate_with_provenance(
        &plan,
        ToolOwnership::default(),
        None,
        Some(&malformed),
    )?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    assert!(
        !body["system"][0]["text"]
            .as_str()
            .ok_or("billing text")?
            .contains("cc_prev_req")
    );
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture checks wake attribution before and after steering"
)]
fn oauth_wake_billing_omits_stale_human_prompt_identity() -> TestResult {
    let resolved = oauth_resolved()?;
    let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"lineage-selected"],
    )?)?)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"lineage-effective"],
    )?)?)?;
    let first = billing_prompt(101, "hello", Provenance::UserInput)?;
    let metadata = RequestMetadata::for_turn(
        RunId::try_from(Uuid::from_u128(102))?,
        TurnId::try_from(Uuid::from_u128(103))?,
        AttemptId::try_from(Uuid::from_u128(104))?,
        Timestamp::new(2, 0)?,
    );
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![first.clone()])?,
        metadata,
    )
    .build()?;
    let first_plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let translator = MessagesRequestTranslator::new(resolved.clone())
        .with_oauth_identity(fixture_oauth_identity()?)
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            99,
        ))?));
    let wake = billing_prompt(
        401,
        "Sub-agent results or questions arrived",
        Provenance::Instruction,
    )?;
    let wake_request = ModelRequest::from_parts(
        first_plan.request().stable_prefix().clone(),
        ConversationView::from_items(vec![
            billing_prompt(201, "earlier human prompt", Provenance::UserInput)?,
            wake,
        ])?,
        metadata,
    )?;
    let wake_plan = resolved
        .descriptor()
        .validate_request(wake_request, ValidationMode::Strict)?;
    assert_eq!(
        wake_plan.request().turn_origin(),
        Some(plakous_model::TurnOrigin::HostWake)
    );
    // A wake still counts as a root turn, but without a native origin
    // spelling it carries no indices either.
    let wake_prepared = translator
        .clone()
        .with_continuation(Some("msg_nc009"), plakous_model::TurnPosition::new(1, 2))
        .translate_with_provenance(&wake_plan, ToolOwnership::default(), None, None)?;
    let wake_body: serde_json::Value = serde_json::from_slice(wake_prepared.body())?;
    let wake_header = wake_body["system"][0]["text"]
        .as_str()
        .ok_or("wake billing")?;
    assert!(!wake_header.contains("cc_prompt_id="));
    assert!(!wake_header.contains("cc_turn_origin="));
    assert!(!wake_header.contains("cc_prompt_index="));
    assert_eq!(
        wake_body["diagnostics"],
        serde_json::json!({"previous_message_id": "msg_nc009"})
    );
    assert_oauth_billing_attested(wake_prepared.body(), &wake_body["system"][0])?;

    for (id, note) in [
        (403, "output limit retry"),
        (404, "operation budget wrap-up"),
    ] {
        let request = ModelRequest::from_parts(
            first_plan.request().stable_prefix().clone(),
            ConversationView::from_shared(
                plakous_model::CanonicalHistory::new(vec![
                    plakous_model::HistorySegment::new(
                        plakous_model::HistorySegmentKind::RecentIntent,
                        vec![billing_prompt(401, "host wake", Provenance::Instruction)?],
                    )?,
                    plakous_model::HistorySegment::new(
                        plakous_model::HistorySegmentKind::RecentIntent,
                        vec![billing_prompt(id, note, Provenance::Instruction)?],
                    )?,
                ])?,
                plakous_model::WorkingProjection::default(),
            ),
            metadata,
        )?;
        let plan = resolved
            .descriptor()
            .validate_request(request, ValidationMode::Strict)?;
        assert_eq!(
            plan.request().turn_origin(),
            Some(plakous_model::TurnOrigin::HostWake)
        );
        let prepared =
            translator.translate_with_provenance(&plan, ToolOwnership::default(), None, None)?;
        let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
        let header = body["system"][0]["text"]
            .as_str()
            .ok_or("harness note billing")?;
        assert!(!header.contains("cc_turn_origin=human"));
        assert!(!header.contains("cc_prompt_id="));
        assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;
    }

    let steered_wake = ModelRequest::from_parts(
        first_plan.request().stable_prefix().clone(),
        ConversationView::from_shared(
            plakous_model::CanonicalHistory::new(vec![
                plakous_model::HistorySegment::new(
                    plakous_model::HistorySegmentKind::RecentIntent,
                    vec![billing_prompt(
                        401,
                        "Sub-agent results arrived",
                        Provenance::Instruction,
                    )?],
                )?,
                plakous_model::HistorySegment::new(
                    plakous_model::HistorySegmentKind::Steering,
                    vec![billing_prompt(
                        402,
                        "Also check the result",
                        Provenance::UserInput,
                    )?],
                )?,
            ])?,
            plakous_model::WorkingProjection::default(),
        ),
        metadata,
    )?;
    let plan = resolved
        .descriptor()
        .validate_request(steered_wake, ValidationMode::Strict)?;
    assert_eq!(
        plan.request().turn_origin(),
        Some(plakous_model::TurnOrigin::HostWake)
    );
    let prepared =
        translator.translate_with_provenance(&plan, ToolOwnership::default(), None, None)?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let header = body["system"][0]["text"]
        .as_str()
        .ok_or("steered wake billing")?;
    assert!(!header.contains("cc_turn_origin=human"));
    assert!(!header.contains("cc_prompt_id="));
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;

    let user_request = ModelRequest::from_parts(
        first_plan.request().stable_prefix().clone(),
        ConversationView::from_shared(
            plakous_model::CanonicalHistory::new(vec![
                plakous_model::HistorySegment::new(
                    plakous_model::HistorySegmentKind::RecentIntent,
                    vec![billing_prompt(501, "human prompt", Provenance::UserInput)?],
                )?,
                plakous_model::HistorySegment::new(
                    plakous_model::HistorySegmentKind::Conversation,
                    vec![billing_prompt(
                        502,
                        "harness round guidance",
                        Provenance::Instruction,
                    )?],
                )?,
            ])?,
            plakous_model::WorkingProjection::default(),
        ),
        metadata,
    )?;
    let plan = resolved
        .descriptor()
        .validate_request(user_request, ValidationMode::Strict)?;
    assert_eq!(
        plan.request().turn_origin(),
        Some(plakous_model::TurnOrigin::UserPrompt)
    );
    let prepared =
        translator.translate_with_provenance(&plan, ToolOwnership::default(), None, None)?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let header = body["system"][0]["text"]
        .as_str()
        .ok_or("user turn billing")?;
    assert!(header.contains("cc_turn_origin=human"));
    assert!(header.contains("cc_prompt_id=00000000-0000-0000-0000-0000000001f5"));
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;

    // A steer and a note appended inside the turn never rename its prompt.
    let steered_user = ModelRequest::from_parts(
        first_plan.request().stable_prefix().clone(),
        ConversationView::from_shared(
            plakous_model::CanonicalHistory::new(vec![
                plakous_model::HistorySegment::new(
                    plakous_model::HistorySegmentKind::RecentIntent,
                    vec![billing_prompt(601, "human prompt", Provenance::UserInput)?],
                )?,
                plakous_model::HistorySegment::new(
                    plakous_model::HistorySegmentKind::Steering,
                    vec![billing_prompt(602, "also this", Provenance::UserInput)?],
                )?,
                plakous_model::HistorySegment::new(
                    plakous_model::HistorySegmentKind::Conversation,
                    vec![billing_prompt(603, "round note", Provenance::UserInput)?],
                )?,
            ])?,
            plakous_model::WorkingProjection::default(),
        ),
        metadata,
    )?;
    let plan = resolved
        .descriptor()
        .validate_request(steered_user, ValidationMode::Strict)?;
    let prepared =
        translator.translate_with_provenance(&plan, ToolOwnership::default(), None, None)?;
    let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
    let header = body["system"][0]["text"]
        .as_str()
        .ok_or("steered user turn billing")?;
    assert!(header.contains("cc_prompt_id=00000000-0000-0000-0000-000000000259;"));
    assert_oauth_billing_attested(prepared.body(), &body["system"][0])?;

    Ok(())
}

fn fixture_oauth_identity()
-> Result<plakous_anthropic::ClaudeOAuthRequestIdentity, plakous_anthropic::AnthropicError> {
    Ok(plakous_anthropic::ClaudeOAuthRequestIdentity::new(
        plakous_anthropic::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc")?,
        plakous_anthropic::ClaudeClientDeviceId::new("ab".repeat(32))?,
    ))
}

#[test]
fn translator_debug_redacts_logical_session_identity() -> TestResult {
    let sentinel = Uuid::from_u128(0xa73b_4321_5678_4def_8012_3456_789a_bcde);
    let translator = MessagesRequestTranslator::new(oauth_resolved()?);
    assert!(format!("{translator:?}").contains("has_session_id: false"));
    let translator =
        translator.with_session_id(Some(plakous_model::SessionId::try_from(sentinel)?));
    for diagnostic in [format!("{translator:?}"), format!("{translator:#?}")] {
        assert!(!diagnostic.contains(&sentinel.to_string()));
        assert!(!diagnostic.contains(&sentinel.simple().to_string()));
        assert!(diagnostic.contains("MessagesRequestTranslator"));
        assert!(diagnostic.contains("resolved:"));
        assert!(diagnostic.contains("has_session_id: true"));
    }
    Ok(())
}

/// Claude Code 2.1.295 emits `context_management` only when
/// `thinking.type !== "disabled"`. Disabled reasoning sends none.
#[test]
fn disabled_thinking_omits_claude_oauth_context_management() -> TestResult {
    for model in ["claude-sonnet-5", "claude-opus-5", "claude-fable-5-1"] {
        for choice in [ToolChoice::None, ToolChoice::Auto] {
            let (resolved, plan) = costume_oauth_plan(
                model,
                ReasoningPolicy::Disabled,
                choice.clone(),
                format!("{model}-disabled-{choice:?}").as_bytes(),
                false,
            )?;
            let prepared = MessagesRequestTranslator::new(resolved)
                .with_oauth_identity(fixture_oauth_identity()?)
                .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
                    99,
                ))?))
                .translate(&plan, ToolOwnership::default())?;
            let body: serde_json::Value = serde_json::from_slice(prepared.body())?;
            assert!(
                body.get("context_management").is_none(),
                "{model} {choice:?} omitted context_management"
            );
        }
    }
    Ok(())
}

/// A dialect built from the Claude OAuth feature pair is the OAuth client
/// class, and its header is the captured baseline, not a feature-set join.
#[test]
fn with_betas_claude_oauth_dialect_keeps_the_baseline_header() {
    let dialect = AnthropicMessagesDialect::with_betas([
        plakous_anthropic::AnthropicBetaFeature::ClaudeOAuth,
        plakous_anthropic::AnthropicBetaFeature::ClaudeCodeClient,
    ]);
    assert!(dialect.is_claude_oauth());
    assert_eq!(
        dialect.beta_header().as_deref(),
        Some(plakous_anthropic::CLAUDE_OAUTH_BASELINE_BETAS)
    );
}
