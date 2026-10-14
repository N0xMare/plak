//! Compile selected Responses behavior into canonical family policy.

use super::{
    CachePolicy, CompiledProviderConfiguration, ConfiguredTool, CustomToolPolicy, Digest,
    EffectiveProviderConfiguration, FamilyPolicyEnvelope, FunctionStrictPolicy,
    MaxOutputTokensPolicy, ModelImplementationIdentity, OutputBoundDisposition, OutputFormat,
    ParallelToolCallsFieldPolicy, ParallelToolDisposition, ProcessingTierDisposition,
    PromptCachePolicy, ProviderStorageDisposition, RESPONSES_ARTIFACT_CODEC_REVISION,
    RESPONSES_ARTIFACT_SCHEMA_REVISION, RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
    RESPONSES_FAMILY_POLICY_SCHEMA_REVISION, RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
    ReasoningEffortDisposition, ReasoningEffortPolicy, ReasoningIncludeDisposition,
    ReasoningIncludePolicy, ReasoningSummaryDisposition, ReasoningSummaryPolicy, ReplayDisposition,
    ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, ResponseStateDisposition,
    ResponsesProviderConfigurationError, ResponsesReplayPolicy, ResponsesRequestPolicy,
    ResponsesRouterPolicy, ResponsesStoragePolicy, RetentionDisposition,
    SelectedProviderConfiguration, Serialize, ServiceTierPolicy, StoreFieldPolicy,
    StructuredOutputPolicy, ToolChoice, ToolChoiceFieldPolicy, ToolExecutionDisposition,
    validate_supported_family_policy,
};

#[derive(Serialize)]
struct CanonicalResponsesPolicy<'a> {
    schema_revision: u32,
    compiler_revision: u32,
    execution_revision: u32,
    artifact_schema_revision: u32,
    artifact_codec_revision: u32,
    execution_identity: Digest,
    service_tier: &'a str,
    max_output_tokens: &'a str,
    function_strict: &'a str,
    custom_tools: &'a str,
    tool_choice: &'a str,
    storage: &'a str,
    store_field: &'a str,
    prompt_cache: &'a str,
    structured_output: &'a str,
    image_input: bool,
    parallel_tool_calls: bool,
    parallel_tool_calls_field: &'a str,
    reasoning_effort: &'a str,
    reasoning_summary: &'a str,
    reasoning_include: &'a str,
    reasoning_include_selector: &'a str,
    replay: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    router_policy: Option<Digest>,
}

/// Compiles one complete selected configuration through one already-resolved Responses policy.
///
/// The provider integration remains the authority for choosing `policy`; this function only
/// validates the selected behavior against that exact table and seals the family artifact.
/// Context evidence_class and local durability are provider-neutral dimensions and are retained
/// unchanged in the effective configuration.
///
/// # Errors
///
/// Returns a field-specific mismatch before neutral route/capability compilation, or the typed
/// neutral configuration error when resolved facts do not match the selection.
pub fn compile_provider_configuration(
    selected: &SelectedProviderConfiguration,
    descriptor: ResolvedModelDescriptor,
    authority: ResolvedProviderAuthorityBinding,
    policy: ResponsesRequestPolicy,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    let implementation = ModelImplementationIdentity::release_owned(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        &descriptor,
        1,
    )?;
    compile_provider_configuration_with_implementation(
        selected,
        descriptor,
        implementation,
        authority,
        policy,
    )
}

/// Compiles one complete selected configuration with an already-validated implementation.
///
/// This is the provider-neutral seam for both release-owned and custom integrations. The
/// neutral configuration compiler rejects an implementation whose integration class is foreign
/// to the resolved descriptor and retains the supplied identity without rewriting it.
///
/// # Errors
///
/// Returns a field-specific policy mismatch or the typed neutral configuration error when the
/// descriptor, implementation, authority, or selected behavior are foreign or inconsistent.
pub fn compile_provider_configuration_with_implementation(
    selected: &SelectedProviderConfiguration,
    descriptor: ResolvedModelDescriptor,
    implementation: ModelImplementationIdentity,
    authority: ResolvedProviderAuthorityBinding,
    policy: ResponsesRequestPolicy,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    compile_provider_configuration_with_implementation_and_router_policy(
        selected,
        descriptor,
        implementation,
        authority,
        policy,
        None,
    )
}

/// Compiles a configuration while binding one exact provider-neutral router policy.
///
/// The router identity enters the canonical Responses family policy and therefore the effective
/// provider configuration, request plan, replay, artifact, and operation identities derived from
/// that configuration. Omitting the router preserves the canonical bytes of existing callers.
///
/// # Errors
///
/// Returns a field-specific policy mismatch or the typed neutral configuration error when the
/// descriptor, implementation, authority, router, or selected behavior are inconsistent.
#[allow(
    clippy::too_many_arguments,
    reason = "the compiler receives independent selected, resolved, implementation, authority, family, and router facts"
)]
pub fn compile_provider_configuration_with_implementation_and_router_policy(
    selected: &SelectedProviderConfiguration,
    descriptor: ResolvedModelDescriptor,
    implementation: ModelImplementationIdentity,
    authority: ResolvedProviderAuthorityBinding,
    policy: ResponsesRequestPolicy,
    router_policy: Option<&ResponsesRouterPolicy>,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    validate_selected_behavior(selected, policy)?;
    let canonical_bytes = canonical_policy_bytes(policy, router_policy)?;
    let family = descriptor.model_family().cloned().ok_or(
        ResponsesProviderConfigurationError::PolicyMismatch("model_family"),
    )?;
    let envelope = FamilyPolicyEnvelope::new(
        family.clone(),
        RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
        RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
        RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
        canonical_bytes,
    )?;
    validate_supported_family_policy(&family, &envelope)?;
    let effective = EffectiveProviderConfiguration::compile(
        selected,
        descriptor.clone(),
        implementation,
        authority,
        selected.behavior().clone(),
        envelope,
    )?;
    Ok(CompiledProviderConfiguration::new(
        selected.clone(),
        effective,
    )?)
}

fn mismatch(field: &'static str) -> ResponsesProviderConfigurationError {
    ResponsesProviderConfigurationError::PolicyMismatch(field)
}

fn validate_selected_behavior(
    selected: &SelectedProviderConfiguration,
    policy: ResponsesRequestPolicy,
) -> Result<(), ResponsesProviderConfigurationError> {
    let behavior = selected.behavior();
    match (&behavior.processing_tier, policy.service_tier) {
        (ProcessingTierDisposition::Standard, _) => {}
        (
            ProcessingTierDisposition::ProviderNamed(selected),
            ServiceTierPolicy::ExactNamed(name),
        ) if selected.as_str() == name => {}
        (ProcessingTierDisposition::ProviderNamed(_), _) => {
            return Err(mismatch("processing_tier"));
        }
    }
    match behavior.reasoning_effort {
        ReasoningEffortDisposition::Disabled => {}
        ReasoningEffortDisposition::Enabled(effort) if policy.reasoning_effort.admits(effort) => {}
        ReasoningEffortDisposition::Enabled(_) => return Err(mismatch("reasoning_effort")),
    }
    let expected_summary = match (behavior.reasoning_effort, policy.reasoning_summary) {
        (ReasoningEffortDisposition::Disabled, _) | (_, ReasoningSummaryPolicy::Omit) => {
            ReasoningSummaryDisposition::Omit
        }
        (_, ReasoningSummaryPolicy::Auto) => ReasoningSummaryDisposition::Auto,
        (_, ReasoningSummaryPolicy::Concise) => ReasoningSummaryDisposition::Concise,
    };
    if behavior.reasoning_summary != expected_summary {
        return Err(mismatch("reasoning_summary"));
    }
    let expected_include = match (behavior.reasoning_effort, policy.reasoning_include) {
        (_, ReasoningIncludePolicy::Always(_))
        | (ReasoningEffortDisposition::Enabled(_), ReasoningIncludePolicy::WhenEnabled(_)) => {
            ReasoningIncludeDisposition::EncryptedContent
        }
        _ => ReasoningIncludeDisposition::Omit,
    };
    if behavior.reasoning_include != expected_include {
        return Err(mismatch("reasoning_include"));
    }
    let expected_replay = match policy.replay {
        ResponsesReplayPolicy::Strict => ReplayDisposition::StrictEncrypted,
        ResponsesReplayPolicy::DropUnreplayableReasoning => {
            ReplayDisposition::DropUnreplayableReasoning
        }
    };
    if behavior.replay != expected_replay {
        return Err(mismatch("replay"));
    }
    match (behavior.cache, policy.prompt_cache) {
        (
            CachePolicy::Disabled,
            PromptCachePolicy::Unsupported
            | PromptCachePolicy::AffinityKey
            | PromptCachePolicy::GatewaySession,
        )
        | (CachePolicy::Enabled { .. }, PromptCachePolicy::Implicit) => {}
        _ => return Err(mismatch("cache")),
    }
    validate_tools(
        &behavior.tools,
        &behavior.tool_choice,
        behavior.parallel_tools,
        policy,
    )?;
    if matches!(behavior.output.format(), OutputFormat::Structured(_))
        && policy.structured_output != StructuredOutputPolicy::StrictJsonSchema
    {
        return Err(mismatch("output"));
    }
    match (behavior.output_bound, policy.max_output_tokens) {
        (OutputBoundDisposition::Enforced(_), MaxOutputTokensPolicy::Emit)
        | (OutputBoundDisposition::NoRequestBound, MaxOutputTokensPolicy::Omit) => {}
        _ => return Err(mismatch("output_bound")),
    }
    let expected_storage = match (policy.storage, policy.store_field) {
        (ResponsesStoragePolicy::ProviderStored, StoreFieldPolicy::Emit) => {
            ProviderStorageDisposition::ProviderStored
        }
        (ResponsesStoragePolicy::Stateless, StoreFieldPolicy::Emit) => {
            ProviderStorageDisposition::DisabledOnWire
        }
        (ResponsesStoragePolicy::Stateless, StoreFieldPolicy::Omit) => {
            ProviderStorageDisposition::OmittedRouteContract
        }
        (ResponsesStoragePolicy::ProviderStored, StoreFieldPolicy::Omit) => {
            return Err(mismatch("storage"));
        }
    };
    if behavior.storage != expected_storage {
        return Err(mismatch("storage"));
    }
    let expected_response_state = match policy.storage {
        ResponsesStoragePolicy::ProviderStored => ResponseStateDisposition::ProviderStored,
        ResponsesStoragePolicy::Stateless => ResponseStateDisposition::Stateless,
    };
    if behavior.response_state != expected_response_state {
        return Err(mismatch("response_state"));
    }
    match &behavior.retention {
        RetentionDisposition::RouteContract { profile, revision }
            if profile == selected.route().integration_profile()
                && revision == selected.route().integration_revision() => {}
        RetentionDisposition::RouteContract { .. } => return Err(mismatch("retention")),
    }
    Ok(())
}

pub(super) fn validate_tools(
    tools: &[ConfiguredTool],
    choice: &ToolChoice,
    parallel: ParallelToolDisposition,
    policy: ResponsesRequestPolicy,
) -> Result<(), ResponsesProviderConfigurationError> {
    if tools
        .iter()
        .any(|tool| tool.execution() != ToolExecutionDisposition::Local)
    {
        return Err(mismatch("tool_ownership"));
    }
    // The selected configuration records canonical local tools. Their model-
    // facing schemas are request-plan facts: an exact V2 projection may replace
    // a canonical UTF-8 operation with a structured provider function. Validate
    // the schemas that actually reach the wire in `lower_tool_definition` /
    // `lower_projected_tool_definition`; compiling against the canonical schema
    // here would reject a valid projection before that authority exists.
    if tools.is_empty() && matches!(choice, ToolChoice::Required | ToolChoice::Specific(_)) {
        return Err(mismatch("tool_choice"));
    }
    // Parallel execution has no meaning when the selected configuration has
    // no tools. The provider capability remains sealed in the resolved
    // descriptor/policy; the explicit behavior for this operation is disabled.
    let expected_parallel = !tools.is_empty() && policy.parallel_tool_calls;
    if matches!(parallel, ParallelToolDisposition::Enabled) != expected_parallel {
        return Err(mismatch("parallel_tools"));
    }
    Ok(())
}

fn canonical_policy_bytes(
    policy: ResponsesRequestPolicy,
    router_policy: Option<&ResponsesRouterPolicy>,
) -> Result<Vec<u8>, ResponsesProviderConfigurationError> {
    let execution_identity = policy
        .execution_identity()
        .map_err(|_| ResponsesProviderConfigurationError::Identity)?;
    let (reasoning_include, reasoning_include_selector) = match policy.reasoning_include {
        ReasoningIncludePolicy::Omit => ("omit", ""),
        ReasoningIncludePolicy::Always(selector) => ("always", selector),
        ReasoningIncludePolicy::WhenEnabled(selector) => ("when-enabled", selector),
    };
    serde_json::to_vec(&CanonicalResponsesPolicy {
        schema_revision: RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
        compiler_revision: RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
        execution_revision: RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
        artifact_schema_revision: RESPONSES_ARTIFACT_SCHEMA_REVISION,
        artifact_codec_revision: RESPONSES_ARTIFACT_CODEC_REVISION,
        execution_identity,
        service_tier: match policy.service_tier {
            ServiceTierPolicy::StandardOnly => "standard-only",
            ServiceTierPolicy::ExactNamed(name) => name,
        },
        max_output_tokens: match policy.max_output_tokens {
            MaxOutputTokensPolicy::Emit => "emit",
            MaxOutputTokensPolicy::Omit => "omit",
        },
        function_strict: match policy.function_strict {
            FunctionStrictPolicy::IncludeTrue => "include-true",
            FunctionStrictPolicy::IncludeFalse => "include-false",
            FunctionStrictPolicy::IncludeNull => "include-null",
            FunctionStrictPolicy::Omit => "omit",
        },
        custom_tools: match policy.custom_tools {
            CustomToolPolicy::StringSchema => "string-schema",
            CustomToolPolicy::Reject => "reject",
        },
        tool_choice: match policy.tool_choice {
            ToolChoiceFieldPolicy::Always => "always",
            ToolChoiceFieldPolicy::WhenToolsPresent => "when-tools-present",
        },
        storage: match policy.storage {
            ResponsesStoragePolicy::ProviderStored => "provider-stored",
            ResponsesStoragePolicy::Stateless => "stateless",
        },
        store_field: match policy.store_field {
            StoreFieldPolicy::Emit => "emit",
            StoreFieldPolicy::Omit => "omit",
        },
        prompt_cache: match policy.prompt_cache {
            PromptCachePolicy::Unsupported => "unsupported",
            PromptCachePolicy::Implicit => "implicit",
            PromptCachePolicy::AffinityKey => "affinity-key",
            PromptCachePolicy::GatewaySession => "gateway-session",
        },
        structured_output: match policy.structured_output {
            StructuredOutputPolicy::StrictJsonSchema => "strict-json-schema",
            StructuredOutputPolicy::Unsupported => "unsupported",
        },
        image_input: policy.image_input,
        parallel_tool_calls: policy.parallel_tool_calls,
        parallel_tool_calls_field: match policy.parallel_tool_calls_field {
            ParallelToolCallsFieldPolicy::WhenToolsPresent => "when-tools-present",
            ParallelToolCallsFieldPolicy::Omit => "omit",
        },
        reasoning_effort: match policy.reasoning_effort {
            ReasoningEffortPolicy::AllNamed => "all-named",
            ReasoningEffortPolicy::LowMediumHigh => "low-medium-high",
            ReasoningEffortPolicy::NoneThroughXHigh => "none-through-xhigh",
        },
        reasoning_summary: match policy.reasoning_summary {
            ReasoningSummaryPolicy::Omit => "omit",
            ReasoningSummaryPolicy::Auto => "auto",
            ReasoningSummaryPolicy::Concise => "concise",
        },
        reasoning_include,
        reasoning_include_selector,
        replay: match policy.replay {
            ResponsesReplayPolicy::Strict => "strict",
            ResponsesReplayPolicy::DropUnreplayableReasoning => "drop-unreplayable-reasoning",
        },
        router_policy: router_policy
            .map(ResponsesRouterPolicy::execution_identity)
            .transpose()
            .map_err(|_| ResponsesProviderConfigurationError::Identity)?,
    })
    .map_err(|_| ResponsesProviderConfigurationError::Identity)
}
