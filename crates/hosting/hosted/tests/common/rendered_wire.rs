//! First-turn HTTP request-body goldens for product routes.
//!
//! Included from `agent.rs` under `#[cfg(test)]`. The inner `#[cfg(test)]`
//! module keeps `tools/checks/check_workspace.py` from treating golden helpers as
//! production CLI code. No production behaviour.

#[cfg(test)]
#[allow(
    clippy::too_many_lines,
    reason = "offline golden renderer walks the full product matrix"
)]
pub(crate) mod suite {
    use std::collections::{BTreeMap, BTreeSet};
    use std::num::NonZeroU32;
    use std::path::Path;

    use plakous::model::{
        Actor, AttemptId, CachePolicy, Capability, CapabilityConstraints, CapabilityRequirement,
        CapabilitySet, CompiledToolProjectionV2, ConfiguredTool, ContentBlock, ConversationItem,
        ConversationView, Digest, DigestDomain, EffectiveProviderConfigurationId, EntryId,
        FactProvenance, FactSource, FeatureSupport, GenerationLimits, InstructionBlock,
        InstructionSource, ModelAdapterProfileId, ModelFamilyId, ModelId, ModelRequest, ProtocolId,
        Provenance, ReasoningEffort, ReasoningPolicy, RequestMetadata, ResolvedModelDescriptor,
        RouteId, RunId, SelectedProviderConfigurationId, Support, TextBlock, Timestamp,
        ToolCallerDisposition, ToolExecutionDisposition, ToolName, TurnId, ValidationMode,
    };
    use plakous_anthropic::{
        CLAUDE_OAUTH_IDENTITY, CLAUDE_OAUTH_MESSAGES_USER_AGENT, CLAUDE_OAUTH_STAINLESS_LANG,
        CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION, CLAUDE_OAUTH_STAINLESS_RETRY_COUNT,
        CLAUDE_OAUTH_STAINLESS_RUNTIME, CLAUDE_OAUTH_STAINLESS_RUNTIME_VERSION,
        CLAUDE_OAUTH_STAINLESS_TIMEOUT, attest_claude_oauth_cch, claude_max_output_tokens,
        claude_oauth_beta_header, claude_oauth_billing_header, claude_oauth_request_max_tokens,
    };
    use plakous_chat_completions::{
        ChatOutputLimitField, ChatReasoningReplay, ChatRequestPolicy, ChatRouterPolicy,
        OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS, ReasoningAliasPolicy, lower_request,
    };
    use plakous_messages::{MessagesRequestPolicy, MessagesRouterPolicy, ToolOwnership};
    use plakous_openai::{
        OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE, OPENAI_CODEX_RESPONSES_PROTOCOL_ID,
    };
    use plakous_openrouter::{
        CHAT_COMPLETIONS_FAMILY_ID, OPENROUTER_CHAT_PROTOCOL_ID, OPENROUTER_MESSAGES_PROTOCOL_ID,
        OPENROUTER_RESPONSES_PROTOCOL_ID, OpenRouterAttribution,
    };
    use plakous_responses_dialect::{
        ResponsesRequestConfig, ResponsesRequestPolicy, ResponsesRouterPolicy,
        lower_responses_request,
    };
    use serde_json::{Map, Value};
    use uuid::Uuid;

    use super::super::NATIVE_RUN_MAX_OUTPUT_TOKENS;
    use super::super::rendered_prompts::suite as prompts;
    use crate::binding::{PlatformProvider, ResolvedProtocol as ProductResolvedProtocol};
    use crate::tools::ProductToolExtensions;

    const TOKEN_COUNT_METHOD: &str = "approximate chars/4 of the compact kernel body; no tiktoken-compatible tokenizer in the workspace";
    const HARNESS_TOKEN_COUNT_METHOD: &str =
        "UTF-8 bytes; approx tokens = bytes / 4; no tiktoken-compatible tokenizer in the workspace";
    const REDACTED: &str = "[redacted]";
    const PLAK_USER_AGENT: &str = concat!("plak/", env!("CARGO_PKG_VERSION"));
    const FIXED_CREATED_AT: u64 = 1_700_000_001;

    pub(crate) struct WireArtifacts {
        pub(crate) request_pretty: String,
        pub(crate) request_jsonc: String,
        pub(crate) headers_pretty: String,
        pub(crate) footprint: HarnessFootprint,
    }

    pub(crate) struct HarnessFootprint {
        pub(crate) instruction_bytes: usize,
        pub(crate) tool_bytes: usize,
        pub(crate) enrolled_source_bytes: usize,
        pub(crate) assignment_bytes: usize,
        pub(crate) instruction_text: String,
    }

    impl HarnessFootprint {
        pub(crate) fn total_bytes(&self) -> usize {
            self.instruction_bytes + self.tool_bytes
        }

        pub(crate) fn approx_tokens(bytes: usize) -> usize {
            bytes / 4
        }

        pub(crate) fn token_count_method() -> &'static str {
            HARNESS_TOKEN_COUNT_METHOD
        }
    }

    fn configuration_id<T>(domain: DigestDomain, seed: &[u8]) -> T
    where
        T: serde::de::DeserializeOwned,
    {
        let digest = Digest::sha256(domain, 5, &[seed]).expect("configuration digest");
        serde_json::from_value(serde_json::to_value(digest).expect("encode digest"))
            .expect("typed configuration identity")
    }

    fn affinity_digest(stem: &str) -> Digest {
        Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"golden-wire-cache-affinity", stem.as_bytes()],
        )
        .expect("cache affinity")
    }

    fn account_binding_digest() -> Digest {
        Digest::sha256(DigestDomain::Blob, 1, &[b"golden-wire-account-binding"])
            .expect("account binding")
    }

    fn first_turn_conversation(spec: &prompts::RouteSpec) -> ConversationView {
        let user_text = prompts::first_turn_user_text(spec);
        ConversationView::from_items(vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(0x10)).expect("user entry"),
                Actor::User,
                vec![ContentBlock::Text(
                    TextBlock::new(&user_text, None).expect("user text"),
                )],
                Provenance::UserInput,
            )
            .expect("user item"),
        ])
        .expect("test conversation items form one valid segment")
    }

    fn turn_metadata() -> RequestMetadata {
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1)).expect("run"),
            TurnId::try_from(Uuid::from_u128(2)).expect("turn"),
            AttemptId::try_from(Uuid::from_u128(3)).expect("attempt"),
            Timestamp::new(FIXED_CREATED_AT, 0).expect("timestamp"),
        )
    }

    fn capability_set(reasoning: bool, caching: bool, parallel: bool) -> CapabilitySet {
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(FIXED_CREATED_AT, 0).expect("capability timestamp"),
            None,
        );
        let native = || {
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(None, None),
                provenance,
            )
        };
        let mut features = BTreeMap::new();
        features.insert(Capability::Streaming, native());
        features.insert(Capability::Tools, native());
        if reasoning {
            features.insert(Capability::Reasoning, native());
        }
        if caching {
            features.insert(Capability::Caching, native());
        }
        if parallel {
            features.insert(Capability::ParallelTools, native());
        }
        CapabilitySet::new(features, BTreeMap::new())
    }

    fn reasoning_for(spec: &prompts::RouteSpec) -> ReasoningPolicy {
        match spec.provider {
            prompts::ProviderKind::OpenRouter { .. } => ReasoningPolicy::Disabled,
            prompts::ProviderKind::Codex => ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::High),
                requirement: CapabilityRequirement::Required,
            },
            prompts::ProviderKind::Grok
            | prompts::ProviderKind::Claude
            | prompts::ProviderKind::Platform(_) => ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Low),
                requirement: CapabilityRequirement::Required,
            },
        }
    }

    fn cache_for(spec: &prompts::RouteSpec) -> CachePolicy {
        match spec.provider {
            prompts::ProviderKind::Claude => CachePolicy::Enabled {
                requirement: CapabilityRequirement::Required,
            },
            _ => CachePolicy::Disabled,
        }
    }

    fn limits_for(spec: &prompts::RouteSpec) -> GenerationLimits {
        let max = match spec.provider {
            prompts::ProviderKind::Codex => None,
            prompts::ProviderKind::Platform(provider) => Some(
                NonZeroU32::new(
                    crate::platform::platform_model(provider, spec.model)
                        .expect("enrolled API row")
                        .output_bound(),
                )
                .expect("API output bound"),
            ),
            _ => Some(NonZeroU32::new(NATIVE_RUN_MAX_OUTPUT_TOKENS).expect("output bound")),
        };
        GenerationLimits::new(max, Vec::new()).expect("generation limits")
    }

    fn configured_tools(
        registry: &plakous::tools::FrozenToolRegistry,
        view: &CompiledToolProjectionV2,
    ) -> Vec<ConfiguredTool> {
        let mut names = BTreeSet::new();
        for definition in view.definitions() {
            for name in definition.covered_canonical_names() {
                names.insert(name.clone());
            }
        }
        names
            .into_iter()
            .map(|name: ToolName| {
                let definition = registry
                    .definition(&name)
                    .unwrap_or_else(|| panic!("canonical tool {} is registered", name.as_str()))
                    .clone();
                ConfiguredTool::new(
                    definition,
                    ToolExecutionDisposition::Local,
                    ToolCallerDisposition::DirectOnly,
                )
            })
            .collect()
    }

    fn instruction_block(
        profile: &plakous::agent::HarnessProfileSpec,
        facts: &[plakous::agent::ResolvedContextSourceFact],
    ) -> InstructionBlock {
        let text = prompts::composed_instruction_text(profile, facts);
        InstructionBlock::new(InstructionSource::Profile, text).expect("instruction block")
    }

    fn build_model_request(
        spec: &prompts::RouteSpec,
        extras: &ProductToolExtensions,
        profile: &plakous::agent::HarnessProfileSpec,
        facts: &[plakous::agent::ResolvedContextSourceFact],
    ) -> ModelRequest {
        let selected: SelectedProviderConfigurationId = configuration_id(
            DigestDomain::SelectedProviderConfiguration,
            format!("golden-wire-selected:{}", spec.file_stem).as_bytes(),
        );
        let effective: EffectiveProviderConfigurationId = configuration_id(
            DigestDomain::EffectiveProviderConfiguration,
            prompts::catalog_authority(spec).as_bytes(),
        );
        let target = prompts::live_projection_target(spec, effective);
        let registry = prompts::registry_for(spec, extras);
        let map = prompts::compile_projection(spec, extras, target)
            .unwrap_or_else(|error| panic!("{}: compile projection: {error}", spec.file_stem));
        let view = map
            .compile_request_view_v2()
            .unwrap_or_else(|error| panic!("{}: compile request view: {error}", spec.file_stem));
        let tools = configured_tools(&registry, &view);
        let request = ModelRequest::builder(
            selected,
            effective,
            first_turn_conversation(spec),
            turn_metadata(),
        )
        .instructions(vec![instruction_block(profile, facts)])
        .tools(tools)
        .reasoning(reasoning_for(spec))
        .cache(cache_for(spec))
        .limits(limits_for(spec))
        .build()
        .unwrap_or_else(|error| panic!("{}: model request: {error}", spec.file_stem));
        let prefix = request
            .stable_prefix()
            .with_tool_projection(view)
            .unwrap_or_else(|error| panic!("{}: bind projection: {error}", spec.file_stem));
        ModelRequest::from_parts(prefix, request.input().clone(), request.metadata())
            .unwrap_or_else(|error| panic!("{}: freeze request: {error}", spec.file_stem))
    }

    fn messages_descriptor(
        protocol: &str,
        route: &str,
        model: &str,
        profile: &str,
        family: &str,
        caching: bool,
    ) -> ResolvedModelDescriptor {
        ResolvedModelDescriptor::new_custom(
            ProtocolId::new(protocol).expect("protocol"),
            RouteId::new(route).expect("route"),
            ModelId::new(model).expect("model"),
            None,
            Some(ModelFamilyId::new(family).expect("family")),
            ModelAdapterProfileId::new(profile).expect("profile"),
            capability_set(true, caching, false),
        )
    }

    fn lower_body(spec: &prompts::RouteSpec, request: ModelRequest) -> (Vec<u8>, bool) {
        let protocol = match spec.provider {
            prompts::ProviderKind::Codex => OPENAI_CODEX_RESPONSES_PROTOCOL_ID,
            prompts::ProviderKind::Grok => "xai.responses.grok-subscription.2026-07",
            prompts::ProviderKind::Claude => "anthropic.messages",
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Chat,
            } => OPENROUTER_CHAT_PROTOCOL_ID,
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Responses,
            } => OPENROUTER_RESPONSES_PROTOCOL_ID,
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Messages,
            } => OPENROUTER_MESSAGES_PROTOCOL_ID,
            prompts::ProviderKind::Platform(PlatformProvider::OpenAi) => {
                plakous_openai::OPENAI_RESPONSES_PROTOCOL_ID
            }
            prompts::ProviderKind::Platform(PlatformProvider::Xai) => {
                "xai.platform.responses.2026-08"
            }
            prompts::ProviderKind::Platform(PlatformProvider::Anthropic) => "anthropic.messages",
        };
        let family = match spec.provider {
            prompts::ProviderKind::Codex
            | prompts::ProviderKind::Grok
            | prompts::ProviderKind::Platform(PlatformProvider::OpenAi | PlatformProvider::Xai) => {
                "responses-family"
            }
            prompts::ProviderKind::Claude
            | prompts::ProviderKind::Platform(PlatformProvider::Anthropic) => "messages-family",
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Chat,
            } => CHAT_COMPLETIONS_FAMILY_ID,
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Responses,
            } => "openrouter.responses",
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Messages,
            } => "openrouter.messages",
        };
        let model = ModelId::new(spec.model).expect("model");
        let protocol_id = ProtocolId::new(protocol).expect("protocol id");
        let family_id = ModelFamilyId::new(family).expect("family id");
        let affinity = affinity_digest(&spec.file_stem);
        let selected = request.selected_provider_configuration_id();
        let effective = request.effective_provider_configuration_id();
        let reasoning = request.reasoning();

        match spec.provider {
            prompts::ProviderKind::Codex
            | prompts::ProviderKind::Grok
            | prompts::ProviderKind::Platform(PlatformProvider::OpenAi | PlatformProvider::Xai)
            | prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Responses,
            } => {
                let policy = match spec.provider {
                    prompts::ProviderKind::Codex => ResponsesRequestPolicy::openai_codex(
                        false,
                        true,
                        OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE,
                    ),
                    prompts::ProviderKind::Grok => ResponsesRequestPolicy::supergrok_stateless(),
                    // The production Platform policies: no cache affinity.
                    prompts::ProviderKind::Platform(PlatformProvider::OpenAi) => {
                        ResponsesRequestPolicy::openai_platform_stateless()
                    }
                    prompts::ProviderKind::Platform(_) => {
                        ResponsesRequestPolicy::xai_platform_stateless(
                            "reasoning.encrypted_content",
                        )
                    }
                    _ => ResponsesRequestPolicy::openrouter_stateless(),
                };
                let platform = matches!(spec.provider, prompts::ProviderKind::Platform(_));
                let config = ResponsesRequestConfig::new(
                    &protocol_id,
                    Some(&family_id),
                    &model,
                    policy,
                    None,
                )
                .with_cache_affinity((!platform).then_some(&affinity));
                let lowered = lower_responses_request(&request, config)
                    .unwrap_or_else(|error| panic!("{}: responses lower: {error}", spec.file_stem));
                let lowered = if matches!(
                    spec.provider,
                    prompts::ProviderKind::OpenRouter {
                        wire: ProductResolvedProtocol::Responses,
                    }
                ) {
                    let router = ResponsesRouterPolicy::gateway_managed(None, None)
                        .expect("openrouter responses router");
                    lowered.with_router_policy(&router).unwrap_or_else(|error| {
                        panic!("{}: responses router: {error}", spec.file_stem)
                    })
                } else {
                    lowered
                };
                let compact = serde_json::to_vec(lowered.body()).expect("responses body bytes");
                (compact, true)
            }
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Chat,
            } => {
                let descriptor = messages_descriptor(
                    protocol,
                    "openrouter.chat.platform:golden-wire",
                    spec.model,
                    "openrouter.chat.dynamic-v1",
                    family,
                    false,
                );
                let plan = descriptor
                    .validate_request(request, ValidationMode::Strict)
                    .unwrap_or_else(|error| panic!("{}: chat validate: {error}", spec.file_stem));
                let router = ChatRouterPolicy::gateway_managed(None, None).expect("chat router");
                let policy = ChatRequestPolicy::new(
                    protocol_id,
                    model,
                    plan.binding_digest(),
                    account_binding_digest(),
                    selected,
                    effective,
                    NATIVE_RUN_MAX_OUTPUT_TOKENS,
                    ChatOutputLimitField::MaxTokens,
                    ChatReasoningReplay::VisibleOnly,
                    Some(router),
                    reasoning,
                    ReasoningAliasPolicy::Reasoning,
                )
                .expect("chat policy")
                .with_tool_choice_none_omits_tools(true)
                .with_plugins(OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS)
                .with_gateway_session_key(affinity.to_string());
                let lowered = lower_request(&plan, &policy)
                    .unwrap_or_else(|error| panic!("{}: chat lower: {error}", spec.file_stem));
                (lowered.body().to_vec(), false)
            }
            prompts::ProviderKind::Claude => {
                let descriptor = messages_descriptor(
                    protocol,
                    "anthropic.claude_oauth",
                    spec.model,
                    "anthropic.claude.oauth.compat.2026-08-20.r1",
                    family,
                    true,
                );
                let plan = descriptor
                    .validate_request(request, ValidationMode::Strict)
                    .unwrap_or_else(|error| panic!("{}: claude validate: {error}", spec.file_stem));
                let account = account_binding_digest();
                let metadata = serde_json::json!({
                    "device_id": "ab".repeat(32),
                    "account_uuid": "12345678-1234-4234-8234-123456789abc",
                    "session_id": plakous::model::SessionId::try_from(Uuid::from_u128(99))
                        .expect("fixture logical session").to_string(),
                })
                .to_string();
                let lineage = match spec.occupancy {
                    prompts::Occupancy::Child | prompts::Occupancy::NestedParent => {
                        plakous::model::SessionLineage::SpawnedChild
                    }
                    prompts::Occupancy::SingleAgent | prompts::Occupancy::Root => {
                        plakous::model::SessionLineage::Root
                    }
                };
                let policy = MessagesRequestPolicy::new(
                    protocol_id,
                    model,
                    None,
                    plan.binding_digest(),
                    descriptor
                        .semantic_binding_digest()
                        .expect("semantic descriptor"),
                    account,
                    selected,
                    effective,
                    claude_max_output_tokens(spec.model),
                    None,
                    "2023-06-01",
                    claude_oauth_beta_header(spec.model),
                )
                .with_system_prefix(claude_oauth_billing_header(lineage, &plan, None, None))
                .with_system_prefix(CLAUDE_OAUTH_IDENTITY)
                .with_serialized_body_transform(attest_claude_oauth_cch)
                .with_metadata_user_id(metadata)
                .with_prompt_cache(true)
                .with_top_level_prompt_cache(false)
                .with_prompt_cache_ttl(Some("1h"))
                .with_omitted_disabled_thinking(true)
                .with_claude_oauth_context_management()
                .with_request_output_cap(
                    claude_oauth_request_max_tokens(spec.model)
                        .expect("enrolled Claude render model"),
                );
                let lowered =
                    plakous_messages::lower_request(&plan, &policy, ToolOwnership::default())
                        .unwrap_or_else(|error| {
                            panic!("{}: claude lower: {error}", spec.file_stem)
                        });
                (lowered.body().to_vec(), false)
            }
            prompts::ProviderKind::Platform(PlatformProvider::Anthropic) => {
                let descriptor = messages_descriptor(
                    protocol,
                    "anthropic.platform",
                    spec.model,
                    plakous_anthropic::ANTHROPIC_API_ADAPTIVE_PROFILE_ID,
                    family,
                    true,
                );
                let plan = descriptor
                    .validate_request(request, ValidationMode::Strict)
                    .unwrap_or_else(|error| {
                        panic!("{}: anthropic-api validate: {error}", spec.file_stem)
                    });
                let policy = MessagesRequestPolicy::new(
                    protocol_id,
                    model,
                    None,
                    plan.binding_digest(),
                    descriptor
                        .semantic_binding_digest()
                        .expect("semantic descriptor"),
                    account_binding_digest(),
                    selected,
                    effective,
                    claude_max_output_tokens(spec.model),
                    None,
                    "2023-06-01",
                    "",
                )
                .with_prompt_cache(true)
                .with_top_level_prompt_cache(false)
                .with_prompt_cache_ttl(Some("1h"));
                let lowered =
                    plakous_messages::lower_request(&plan, &policy, ToolOwnership::default())
                        .unwrap_or_else(|error| {
                            panic!("{}: anthropic-api lower: {error}", spec.file_stem)
                        });
                (lowered.body().to_vec(), false)
            }
            prompts::ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Messages,
            } => {
                let descriptor = messages_descriptor(
                    protocol,
                    "openrouter.messages.platform:golden-wire",
                    spec.model,
                    "openrouter.messages.dynamic-custom-v1",
                    family,
                    false,
                );
                let plan = descriptor
                    .validate_request(request, ValidationMode::Strict)
                    .unwrap_or_else(|error| {
                        panic!("{}: openrouter-messages validate: {error}", spec.file_stem)
                    });
                let router = MessagesRouterPolicy::gateway_managed(None, None)
                    .expect("openrouter messages router");
                let policy = MessagesRequestPolicy::new(
                    protocol_id,
                    model,
                    None,
                    plan.binding_digest(),
                    descriptor
                        .semantic_binding_digest()
                        .expect("semantic descriptor"),
                    account_binding_digest(),
                    selected,
                    effective,
                    NATIVE_RUN_MAX_OUTPUT_TOKENS,
                    None,
                    "openrouter-messages-v1",
                    "",
                )
                .with_router_policy(router)
                .with_plugins(plakous_messages::OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS)
                .with_prompt_cache(true)
                .with_top_level_prompt_cache(false)
                .with_session_id(affinity.to_string());
                let lowered =
                    plakous_messages::lower_request(&plan, &policy, ToolOwnership::default())
                        .unwrap_or_else(|error| {
                            panic!("{}: openrouter-messages lower: {error}", spec.file_stem)
                        });
                (lowered.body().to_vec(), false)
            }
        }
    }

    pub(crate) fn review_headers(spec: &prompts::RouteSpec) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        match spec.provider {
            prompts::ProviderKind::Codex => {
                headers.push(("authorization".into(), REDACTED.into()));
                headers.push(("chatgpt-account-id".into(), REDACTED.into()));
                headers.push(("originator".into(), "plak".into()));
                headers.push((
                    "session-id".into(),
                    affinity_digest(&spec.file_stem).to_string(),
                ));
                headers.push(("user-agent".into(), PLAK_USER_AGENT.into()));
            }
            prompts::ProviderKind::Grok => {
                headers.push(("user-agent".into(), PLAK_USER_AGENT.into()));
                headers.push(("x-xai-token-auth".into(), "xai-grok-cli".into()));
                headers.push((
                    "x-authenticateresponse".into(),
                    "authenticate-response".into(),
                ));
                headers.push(("x-grok-client-version".into(), "1.0.46".into()));
                headers.push(("x-grok-client-identifier".into(), "plakous".into()));
                headers.push(("x-grok-client-mode".into(), "interactive".into()));
            }
            prompts::ProviderKind::Claude => {
                let attempt = AttemptId::try_from(Uuid::from_u128(3))
                    .expect("attempt")
                    .to_string();
                headers.push(("authorization".into(), REDACTED.into()));
                headers.push((
                    "anthropic-beta".into(),
                    claude_oauth_beta_header(spec.model).into(),
                ));
                headers.push(("user-agent".into(), CLAUDE_OAUTH_MESSAGES_USER_AGENT.into()));
                headers.push(("anthropic-version".into(), "2023-06-01".into()));
                headers.push((
                    "anthropic-dangerous-direct-browser-access".into(),
                    "true".into(),
                ));
                headers.push(("x-app".into(), "cli".into()));
                headers.push(("x-stainless-arch".into(), "arm64".into()));
                headers.push((
                    "x-stainless-lang".into(),
                    CLAUDE_OAUTH_STAINLESS_LANG.into(),
                ));
                headers.push(("x-stainless-os".into(), "MacOS".into()));
                headers.push((
                    "x-stainless-package-version".into(),
                    CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION.into(),
                ));
                headers.push((
                    "x-stainless-retry-count".into(),
                    CLAUDE_OAUTH_STAINLESS_RETRY_COUNT.into(),
                ));
                headers.push((
                    "x-stainless-runtime".into(),
                    CLAUDE_OAUTH_STAINLESS_RUNTIME.into(),
                ));
                headers.push((
                    "x-stainless-runtime-version".into(),
                    CLAUDE_OAUTH_STAINLESS_RUNTIME_VERSION.into(),
                ));
                headers.push((
                    "x-stainless-timeout".into(),
                    CLAUDE_OAUTH_STAINLESS_TIMEOUT.into(),
                ));
                headers.push(("x-client-request-id".into(), attempt.clone()));
                headers.push(("x-claude-code-session-id".into(), attempt));
                headers.push(("content-type".into(), "application/json".into()));
                headers.push(("accept".into(), "application/json".into()));
            }
            // No client identity on an API route: the pack without its costume.
            prompts::ProviderKind::Platform(PlatformProvider::Anthropic) => {
                headers.push(("x-api-key".into(), REDACTED.into()));
                headers.push(("anthropic-version".into(), "2023-06-01".into()));
                headers.push(("content-type".into(), "application/json".into()));
                headers.push(("accept".into(), "text/event-stream".into()));
            }
            prompts::ProviderKind::Platform(_) => {
                headers.push(("accept".into(), "text/event-stream".into()));
                headers.push(("content-type".into(), "application/json".into()));
                headers.push(("authorization".into(), REDACTED.into()));
            }
            prompts::ProviderKind::OpenRouter { .. } => {
                headers.push(("accept".into(), "text/event-stream".into()));
                headers.push(("content-type".into(), "application/json".into()));
                headers.push(("authorization".into(), REDACTED.into()));
                headers.push(("x-openrouter-metadata".into(), "enabled".into()));
                headers.push((
                    "http-referer".into(),
                    OpenRouterAttribution.referer().into(),
                ));
                headers.push((
                    "x-openrouter-title".into(),
                    OpenRouterAttribution.title().into(),
                ));
            }
        }
        for (name, value) in &mut headers {
            if name == "user-agent" && value.starts_with("plak/") {
                assert_eq!(value, PLAK_USER_AGENT);
                let version = value.strip_prefix("plak/").expect("Plak UA prefix");
                let parts: Vec<_> = version.split('.').collect();
                assert_eq!(parts.len(), 3, "Plak UA must contain a semver");
                assert!(parts.iter().all(|part| part.parse::<u64>().is_ok()));
                *value = "plak/<version>".into();
            }
        }
        headers
    }

    fn headers_document(spec: &prompts::RouteSpec, compact_body: &[u8]) -> Value {
        let chars = compact_body.len();
        let headers = review_headers(spec)
            .into_iter()
            .map(|(name, value)| {
                let mut object = Map::new();
                object.insert("name".into(), Value::String(name));
                object.insert("value".into(), Value::String(value));
                Value::Object(object)
            })
            .collect();
        let mut body = Map::new();
        body.insert("chars".into(), Value::from(chars));
        body.insert("approx_tokens".into(), Value::from(chars / 4));
        body.insert(
            "token_count_method".into(),
            Value::String(TOKEN_COUNT_METHOD.into()),
        );
        let mut root = Map::new();
        root.insert("headers".into(), Value::Array(headers));
        root.insert("body".into(), Value::Object(body));
        Value::Object(root)
    }

    fn pretty_json_preserving_order(compact: &[u8]) -> String {
        let text = std::str::from_utf8(compact).expect("utf-8 json");
        let mut out = String::new();
        pretty_value(text.as_bytes(), &mut 0, 0, &mut out);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    fn skip_ws(src: &[u8], index: &mut usize) {
        while *index < src.len() && src[*index].is_ascii_whitespace() {
            *index += 1;
        }
    }

    fn pretty_value(src: &[u8], index: &mut usize, indent: usize, out: &mut String) {
        skip_ws(src, index);
        match src.get(*index).copied() {
            Some(b'{') => pretty_object(src, index, indent, out),
            Some(b'[') => pretty_array(src, index, indent, out),
            Some(b'"') => out.push_str(&take_string(src, index)),
            Some(b't') | Some(b'f') | Some(b'n') | Some(b'-') | Some(b'0'..=b'9') => {
                out.push_str(&take_atom(src, index));
            }
            other => panic!("unexpected json at {index}: {other:?}"),
        }
    }

    fn pretty_object(src: &[u8], index: &mut usize, indent: usize, out: &mut String) {
        *index += 1;
        skip_ws(src, index);
        out.push('{');
        if src.get(*index) == Some(&b'}') {
            *index += 1;
            out.push('}');
            return;
        }
        out.push('\n');
        let mut first = true;
        loop {
            skip_ws(src, index);
            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&"  ".repeat(indent + 1));
            let key = take_string(src, index);
            out.push_str(&key);
            skip_ws(src, index);
            assert_eq!(src.get(*index).copied(), Some(b':'), "expected colon");
            *index += 1;
            out.push_str(": ");
            pretty_value(src, index, indent + 1, out);
            skip_ws(src, index);
            match src.get(*index).copied() {
                Some(b',') => {
                    *index += 1;
                }
                Some(b'}') => {
                    *index += 1;
                    out.push('\n');
                    out.push_str(&"  ".repeat(indent));
                    out.push('}');
                    return;
                }
                other => panic!("unexpected object token {other:?}"),
            }
        }
    }

    fn pretty_array(src: &[u8], index: &mut usize, indent: usize, out: &mut String) {
        *index += 1;
        skip_ws(src, index);
        out.push('[');
        if src.get(*index) == Some(&b']') {
            *index += 1;
            out.push(']');
            return;
        }
        out.push('\n');
        let mut first = true;
        loop {
            skip_ws(src, index);
            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&"  ".repeat(indent + 1));
            pretty_value(src, index, indent + 1, out);
            skip_ws(src, index);
            match src.get(*index).copied() {
                Some(b',') => {
                    *index += 1;
                }
                Some(b']') => {
                    *index += 1;
                    out.push('\n');
                    out.push_str(&"  ".repeat(indent));
                    out.push(']');
                    return;
                }
                other => panic!("unexpected array token {other:?}"),
            }
        }
    }

    fn take_string(src: &[u8], index: &mut usize) -> String {
        assert_eq!(src.get(*index).copied(), Some(b'"'));
        let start = *index;
        *index += 1;
        while *index < src.len() {
            match src[*index] {
                b'\\' => *index += 2,
                b'"' => {
                    *index += 1;
                    return String::from_utf8(src[start..*index].to_vec()).expect("string");
                }
                _ => *index += 1,
            }
        }
        panic!("unterminated string");
    }

    fn take_atom(src: &[u8], index: &mut usize) -> String {
        let start = *index;
        while *index < src.len() {
            match src[*index] {
                b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t' => break,
                _ => *index += 1,
            }
        }
        String::from_utf8(src[start..*index].to_vec()).expect("atom")
    }

    fn pretty_jsonc(value: &Value) -> String {
        let mut out = String::new();
        pretty_jsonc_value(value, 0, &mut out);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    fn pretty_jsonc_value(value: &Value, indent: usize, out: &mut String) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(boolean) => out.push_str(if *boolean { "true" } else { "false" }),
            Value::Number(number) => out.push_str(&number.to_string()),
            Value::String(text) => pretty_jsonc_string(text, out),
            Value::Array(items) => pretty_jsonc_array(items, indent, out),
            Value::Object(map) => pretty_jsonc_object(map, indent, out),
        }
    }

    fn pretty_jsonc_string(text: &str, out: &mut String) {
        out.push('"');
        for ch in text.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push('\n'),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                ch if ch.is_control() => {
                    let code = ch as u32;
                    out.push_str(&format!("\\u{code:04x}"));
                }
                ch => out.push(ch),
            }
        }
        out.push('"');
    }

    fn pretty_jsonc_array(items: &[Value], indent: usize, out: &mut String) {
        out.push('[');
        if items.is_empty() {
            out.push(']');
            return;
        }
        out.push('\n');
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&"  ".repeat(indent + 1));
            pretty_jsonc_value(item, indent + 1, out);
        }
        out.push('\n');
        out.push_str(&"  ".repeat(indent));
        out.push(']');
    }

    fn pretty_jsonc_object(map: &Map<String, Value>, indent: usize, out: &mut String) {
        out.push('{');
        if map.is_empty() {
            out.push('}');
            return;
        }
        out.push('\n');
        for (index, (key, value)) in map.iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&"  ".repeat(indent + 1));
            pretty_jsonc_string(key, out);
            out.push_str(": ");
            pretty_jsonc_value(value, indent + 1, out);
        }
        out.push('\n');
        out.push_str(&"  ".repeat(indent));
        out.push('}');
    }

    /// `pub(crate)` so Markdown goldens embed the same kernel body as
    /// `target/prompt-renders/rendered/wire/<route>.request.json`.
    pub(crate) fn lowered_request_value(
        spec: &prompts::RouteSpec,
        extras: &ProductToolExtensions,
        profile: &plakous::agent::HarnessProfileSpec,
        facts: &[plakous::agent::ResolvedContextSourceFact],
    ) -> Value {
        let request = build_model_request(spec, extras, profile, facts);
        let (compact, _) = lower_body(spec, request);
        serde_json::from_slice(&compact).expect("wire body json")
    }

    fn responses_message_text(message: &Value) -> Option<&str> {
        let content = message.get("content")?;
        match content {
            Value::String(text) => Some(text.as_str()),
            Value::Array(parts) => parts.iter().find_map(|part| match part {
                Value::String(text) => Some(text.as_str()),
                Value::Object(_) => part.get("text").and_then(Value::as_str),
                _ => None,
            }),
            _ => None,
        }
    }

    fn message_text(message: &Value) -> Option<&str> {
        match message.get("content") {
            Some(Value::String(text)) => Some(text.as_str()),
            Some(Value::Array(parts)) => parts.iter().find_map(|part| match part {
                Value::String(text) => Some(text.as_str()),
                Value::Object(_) => part.get("text").and_then(Value::as_str),
                _ => None,
            }),
            _ => None,
        }
    }

    pub(crate) fn first_user_turn(body: &Value) -> Option<String> {
        for key in ["messages", "input"] {
            let Some(items) = body.get(key).and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                if item.get("role").and_then(Value::as_str) == Some("user") {
                    if let Some(text) = message_text(item).or_else(|| responses_message_text(item))
                    {
                        return Some(text.to_owned());
                    }
                }
            }
        }
        None
    }

    fn collect_instruction_texts(body: &Value) -> Vec<String> {
        let mut texts = Vec::new();
        if let Some(text) = body.get("instructions").and_then(Value::as_str) {
            texts.push(text.to_owned());
        }
        match body.get("system") {
            Some(Value::String(text)) => texts.push(text.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        texts.push(text.to_owned());
                    } else if let Some(text) = block.as_str() {
                        texts.push(text.to_owned());
                    }
                }
            }
            _ => {}
        }
        for key in ["messages", "input"] {
            let Some(items) = body.get(key).and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                // Codex carries instructions as an identified developer message.
                let role = item.get("role").and_then(Value::as_str);
                let codex_instructions = role == Some("developer")
                    && item
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with("msg_"));
                if role == Some("system") || codex_instructions {
                    if let Some(text) = message_text(item).or_else(|| responses_message_text(item))
                    {
                        texts.push(text.to_owned());
                    }
                }
            }
        }
        texts
    }

    fn strip_enrolled_sources(
        text: &str,
        facts: &[plakous::agent::ResolvedContextSourceFact],
    ) -> (String, usize) {
        let mut remaining = text.to_owned();
        let mut removed = 0usize;
        for fact in facts.iter().rev() {
            let content = fact.content();
            if content.is_empty() {
                continue;
            }
            let separated = format!("\n\n{content}");
            if remaining.ends_with(&separated) {
                remaining.truncate(remaining.len() - separated.len());
                removed += separated.len();
            } else if remaining.ends_with(content) {
                remaining.truncate(remaining.len() - content.len());
                removed += content.len();
            }
        }
        (remaining, removed)
    }

    fn skip_value(src: &[u8], index: &mut usize) {
        let mut sink = String::new();
        pretty_value(src, index, 0, &mut sink);
    }

    fn top_level_member_slice<'a>(src: &'a [u8], want: &str) -> Option<&'a str> {
        let mut index = 0usize;
        skip_ws(src, &mut index);
        if src.get(index) != Some(&b'{') {
            return None;
        }
        index += 1;
        skip_ws(src, &mut index);
        if src.get(index) == Some(&b'}') {
            return None;
        }
        loop {
            skip_ws(src, &mut index);
            let key = take_string(src, &mut index);
            skip_ws(src, &mut index);
            assert_eq!(src.get(index).copied(), Some(b':'), "expected colon");
            index += 1;
            skip_ws(src, &mut index);
            let start = index;
            skip_value(src, &mut index);
            let matched = key.len() >= 2 && &key[1..key.len() - 1] == want;
            if matched {
                return Some(std::str::from_utf8(&src[start..index]).expect("utf-8 json member"));
            }
            skip_ws(src, &mut index);
            match src.get(index).copied() {
                Some(b',') => {
                    index += 1;
                }
                Some(b'}') => return None,
                other => panic!("unexpected object token {other:?}"),
            }
        }
    }

    fn harness_footprint(
        compact: &[u8],
        body: &Value,
        facts: &[plakous::agent::ResolvedContextSourceFact],
    ) -> HarnessFootprint {
        let mut instruction_text = String::new();
        let mut enrolled_source_bytes = 0usize;
        for part in collect_instruction_texts(body) {
            let (stripped, removed) = strip_enrolled_sources(&part, facts);
            enrolled_source_bytes += removed;
            instruction_text.push_str(&stripped);
        }
        let tool_bytes = top_level_member_slice(compact, "tools")
            .map(str::len)
            .unwrap_or(0);
        let assignment_bytes = first_user_turn(body).map(|text| text.len()).unwrap_or(0);
        HarnessFootprint {
            instruction_bytes: instruction_text.len(),
            tool_bytes,
            enrolled_source_bytes,
            assignment_bytes,
            instruction_text,
        }
    }

    pub(crate) fn render_wire(
        spec: &prompts::RouteSpec,
        home: &Path,
        root: &Path,
        cwd: &Path,
        tools: &prompts::OccupancyTools,
    ) -> WireArtifacts {
        let extras = prompts::extras_for(spec, tools);
        let (profile, facts) = prompts::assemble_profile(spec, &extras, home, root, cwd);
        let request = build_model_request(spec, &extras, &profile, &facts);
        let (compact, btree_ordered) = lower_body(spec, request);
        let request_pretty = if btree_ordered {
            let value: Value = serde_json::from_slice(&compact).expect("responses json");
            let mut pretty = serde_json::to_string_pretty(&value).expect("pretty responses");
            if !pretty.ends_with('\n') {
                pretty.push('\n');
            }
            pretty
        } else {
            pretty_json_preserving_order(&compact)
        };
        let request_jsonc =
            pretty_jsonc(&serde_json::from_slice::<Value>(&compact).expect("wire body value"));
        let body_value: Value = serde_json::from_slice(&compact).expect("wire body value");
        let footprint = harness_footprint(&compact, &body_value, &facts);
        let headers_pretty = {
            let mut pretty =
                serde_json::to_string_pretty(&headers_document(spec, &compact)).expect("headers");
            if !pretty.ends_with('\n') {
                pretty.push('\n');
            }
            pretty
        };
        WireArtifacts {
            request_pretty,
            request_jsonc,
            headers_pretty,
            footprint,
        }
    }

    #[test]
    fn child_wire_first_turn_is_assignment_start() {
        let (_tmp, home, root, cwd) = prompts::instruction_fixture();
        let tools = prompts::occupancy_tools(&root);
        let child_routes: Vec<_> = prompts::routes()
            .into_iter()
            .filter(|route| route.occupancy == prompts::Occupancy::Child)
            .collect();
        assert!(!child_routes.is_empty(), "matrix includes child routes");
        for spec in child_routes {
            let extras = prompts::extras_for(&spec, &tools);
            let (profile, facts) = prompts::assemble_profile(&spec, &extras, &home, &root, &cwd);
            let body = lowered_request_value(&spec, &extras, &profile, &facts);
            let expected = prompts::child_assignment_start_prompt(spec.composition);
            let actual = first_user_turn(&body)
                .unwrap_or_else(|| panic!("{}: wire body missing first user turn", spec.file_stem));
            assert_eq!(
                actual, expected,
                "{}: child first user turn must be assignment_start_prompt",
                spec.file_stem
            );
        }
    }

    #[test]
    fn nested_parent_wire_first_turn_is_assignment_start() {
        let (_tmp, home, root, cwd) = prompts::instruction_fixture();
        let tools = prompts::occupancy_tools(&root);
        let routes: Vec<_> = prompts::routes()
            .into_iter()
            .filter(|route| route.occupancy == prompts::Occupancy::NestedParent)
            .collect();
        assert!(!routes.is_empty(), "matrix includes nested-parent routes");
        for spec in routes {
            let extras = prompts::extras_for(&spec, &tools);
            let names: Vec<_> = extras.names().map(ToolName::as_str).collect();
            assert!(
                names.contains(&"spawn_agent"),
                "{}: nested-parent must expose spawn_agent",
                spec.file_stem
            );
            assert!(
                names.contains(&"submit_result"),
                "{}: nested-parent must expose submit_result",
                spec.file_stem
            );
            let (profile, facts) = prompts::assemble_profile(&spec, &extras, &home, &root, &cwd);
            let body = lowered_request_value(&spec, &extras, &profile, &facts);
            let expected = prompts::nested_parent_assignment_start_prompt(spec.composition);
            let actual = first_user_turn(&body)
                .unwrap_or_else(|| panic!("{}: wire body missing first user turn", spec.file_stem));
            assert_eq!(
                actual, expected,
                "{}: nested-parent first user turn must be orchestrate assignment_start_prompt",
                spec.file_stem
            );
        }
    }

    #[test]
    fn grok_direct_wire_lowers_instructions_as_first_input_system_message() {
        let (_tmp, home, root, cwd) = prompts::instruction_fixture();
        let tools = prompts::occupancy_tools(&root);
        let spec = prompts::routes()
            .into_iter()
            .find(|route| route.file_stem == "grok.direct.root")
            .expect("grok.direct.root route");
        let extras = prompts::extras_for(&spec, &tools);
        let (profile, facts) = prompts::assemble_profile(&spec, &extras, &home, &root, &cwd);
        let body = lowered_request_value(&spec, &extras, &profile, &facts);
        assert!(
            body.get("instructions").is_none(),
            "SuperGrok wire must omit top-level instructions"
        );
        let input = body["input"].as_array().expect("responses input array");
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "system");
    }

    #[test]
    fn rendered_product_wire_matches_goldens() {
        let (_tmp, home, root, cwd) = prompts::instruction_fixture();
        let tools = prompts::occupancy_tools(&root);
        let update = std::env::var("PLAK_UPDATE_GOLDENS").is_ok_and(|value| value == "1");
        let wire_dir = prompts::wire_dir();
        if update {
            std::fs::create_dir_all(&wire_dir).expect("create prompt wire artifact dir");
        }

        let mut failures = Vec::new();
        for spec in prompts::routes() {
            let rendered = render_wire(&spec, &home, &root, &cwd, &tools);
            let request: Value = serde_json::from_str(&rendered.request_pretty)
                .expect("rendered wire request is JSON");
            let headers: Value = serde_json::from_str(&rendered.headers_pretty)
                .expect("rendered wire headers are JSON");
            let extras = prompts::extras_for(&spec, &tools);
            let (profile, facts) = prompts::assemble_profile(&spec, &extras, &home, &root, &cwd);
            if request != lowered_request_value(&spec, &extras, &profile, &facts) {
                failures.push(format!(
                    "{}: rendered wire request does not match the kernel-lowered request",
                    spec.file_stem
                ));
            }
            if headers.as_object().is_none_or(Map::is_empty) {
                failures.push(format!(
                    "{}: rendered wire headers are not a non-empty object",
                    spec.file_stem
                ));
            }
            if update {
                std::fs::write(
                    wire_dir.join(format!("{}.request.json", spec.file_stem)),
                    &rendered.request_pretty,
                )
                .expect("write wire request artifact");
                std::fs::write(
                    wire_dir.join(format!("{}.headers.json", spec.file_stem)),
                    &rendered.headers_pretty,
                )
                .expect("write wire headers artifact");
            }
        }
        assert!(
            failures.is_empty(),
            "rendered wire drift (inspect with `just prompts-diff`):\n\n{}",
            failures.join("\n\n")
        );
    }
}
