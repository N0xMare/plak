//! Exact selected-to-effective configuration compiler for the Messages family.

use plakous_model::{
    CachePolicy, CompiledProviderConfiguration, ContextAdmissionRequirement,
    EffectiveProviderConfiguration, FamilyPolicyEnvelope, HostedTool, LocalDurabilityDisposition,
    OutputBoundDisposition, OutputFormat, ParallelToolDisposition, ProviderConfigurationError,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, ResponseStateDisposition, RetentionDisposition,
    SelectedProviderConfiguration, ToolChoice, ToolExecutionDisposition,
};
use serde::Serialize;

/// Messages family-policy schema revision.
pub const MESSAGES_FAMILY_POLICY_SCHEMA_REVISION: u32 = 1;
/// Messages selected-to-effective compiler revision.
pub const MESSAGES_FAMILY_POLICY_COMPILER_REVISION: u32 = 1;
/// Exact baseline request behavior revision.
pub const MESSAGES_REQUEST_POLICY_REVISION: u32 = 3;

#[derive(Serialize)]
struct Policy<'a> {
    api_version: &'a str,
    beta_header: &'a str,
    stream: bool,
    thinking: &'a str,
    cache: &'a str,
    storage: &'a str,
    aggregate_continuation: &'a str,
    managed_cache_artifact: &'a str,
    native_compaction: &'a str,
    item_replay_schema_revision: u32,
}

/// Compiles the conservative, stateless, thinking-disabled Messages behavior.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the compiler consumes the resolved descriptor into the effective configuration"
)]
pub fn compile_messages_configuration(
    selected: &SelectedProviderConfiguration,
    descriptor: ResolvedModelDescriptor,
    authority: ResolvedProviderAuthorityBinding,
    api_version: &str,
    beta_header: &str,
) -> Result<CompiledProviderConfiguration, MessagesProviderConfigurationError> {
    compile_messages_configuration_inner(
        selected,
        descriptor,
        authority,
        api_version,
        beta_header,
        false,
    )
}

/// Checks stable adaptive API behavior before a provider reads credentials.
pub fn validate_messages_api_behavior(
    behavior: &plakous_model::ProviderBehaviorConfiguration,
) -> Result<(), MessagesProviderConfigurationError> {
    validate_messages_behavior(behavior, "2023-06-01", "", true)
}

/// Compiles the stable API adaptive-thinking policy with caller-owned capacity.
pub fn compile_messages_api_configuration(
    selected: &SelectedProviderConfiguration,
    descriptor: ResolvedModelDescriptor,
    authority: ResolvedProviderAuthorityBinding,
) -> Result<CompiledProviderConfiguration, MessagesProviderConfigurationError> {
    compile_messages_configuration_inner(selected, descriptor, authority, "2023-06-01", "", true)
}

fn compile_messages_configuration_inner(
    selected: &SelectedProviderConfiguration,
    descriptor: ResolvedModelDescriptor,
    authority: ResolvedProviderAuthorityBinding,
    api_version: &str,
    beta_header: &str,
    adaptive_api: bool,
) -> Result<CompiledProviderConfiguration, MessagesProviderConfigurationError> {
    let behavior = selected.behavior();
    validate_messages_behavior(behavior, api_version, beta_header, adaptive_api)?;
    let family = descriptor
        .model_family()
        .cloned()
        .ok_or(MessagesProviderConfigurationError::PolicyMismatch)?;
    let hosted_artifact = behavior
        .hosted_tools
        .contains(&HostedTool::ProgrammaticComposition);
    let bytes = serde_json::to_vec(&Policy {
        api_version,
        beta_header,
        stream: true,
        thinking: if adaptive_api { "adaptive" } else { "disabled" },
        cache: if adaptive_api {
            "ephemeral-1h-breakpoints"
        } else {
            "disabled"
        },
        storage: "stateless-omitted-route-contract",
        aggregate_continuation: if hosted_artifact {
            "messages-hosted-artifact-v1"
        } else {
            "absent"
        },
        managed_cache_artifact: "absent",
        native_compaction: "absent",
        item_replay_schema_revision: crate::MESSAGES_REPLAY_SCHEMA_REVISION,
    })
    .map_err(|_| MessagesProviderConfigurationError::Identity)?;
    let policy = FamilyPolicyEnvelope::new(
        family,
        MESSAGES_FAMILY_POLICY_SCHEMA_REVISION,
        MESSAGES_FAMILY_POLICY_COMPILER_REVISION,
        MESSAGES_REQUEST_POLICY_REVISION,
        bytes,
    )?;
    let implementation = plakous_model::ModelImplementationIdentity::release_owned(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        &descriptor,
        1,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        selected,
        descriptor,
        implementation,
        authority,
        behavior.clone(),
        policy,
    )?;
    Ok(CompiledProviderConfiguration::new(
        selected.clone(),
        effective,
    )?)
}

fn validate_messages_behavior(
    behavior: &plakous_model::ProviderBehaviorConfiguration,
    api_version: &str,
    beta_header: &str,
    adaptive_api: bool,
) -> Result<(), MessagesProviderConfigurationError> {
    let exact = matches!(
        behavior.processing_tier,
        plakous_model::ProcessingTierDisposition::Standard
    ) && (if adaptive_api {
        matches!(
            behavior.reasoning_effort,
            ReasoningEffortDisposition::Enabled(
                plakous_model::ReasoningEffort::Low
                    | plakous_model::ReasoningEffort::Medium
                    | plakous_model::ReasoningEffort::High
                    | plakous_model::ReasoningEffort::XHigh
                    | plakous_model::ReasoningEffort::Max
            )
        ) && behavior.hosted_tools.is_empty()
    } else {
        matches!(
            behavior.reasoning_effort,
            ReasoningEffortDisposition::Disabled
        )
    }) && matches!(
        behavior.reasoning_summary,
        ReasoningSummaryDisposition::Omit
    ) && matches!(
        behavior.reasoning_include,
        ReasoningIncludeDisposition::Omit
    ) && matches!(behavior.replay, ReplayDisposition::StrictEncrypted)
        && (adaptive_api || matches!(behavior.cache, CachePolicy::Disabled))
        && matches!(
            behavior.parallel_tools,
            ParallelToolDisposition::Enabled | ParallelToolDisposition::Disabled
        )
        && matches!(behavior.output.format(), OutputFormat::Text)
        && matches!(behavior.output_bound, OutputBoundDisposition::Enforced(_))
        && matches!(
            behavior.storage,
            ProviderStorageDisposition::OmittedRouteContract
        )
        && matches!(behavior.response_state, ResponseStateDisposition::Stateless)
        && (if adaptive_api {
            matches!(
                behavior.context_evidence_requirement,
                ContextAdmissionRequirement::AnyBounded
            )
        } else {
            matches!(
                behavior.context_evidence_requirement,
                ContextAdmissionRequirement::ReleaseVerifiedRequired
            )
        })
        && matches!(
            behavior.local_durability,
            LocalDurabilityDisposition::Ephemeral
                | LocalDurabilityDisposition::BestEffort
                | LocalDurabilityDisposition::DurableRequired
        )
        && matches!(
            behavior.retention,
            RetentionDisposition::RouteContract { .. }
        )
        && !matches!(
            behavior.tool_choice,
            ToolChoice::Required | ToolChoice::Specific(_)
        )
        && behavior
            .tools
            .iter()
            .all(|tool| tool.execution() == ToolExecutionDisposition::Local)
        && api_version == "2023-06-01"
        && beta_header.is_empty();
    if !exact {
        return Err(MessagesProviderConfigurationError::PolicyMismatch);
    }
    Ok(())
}

/// Compiles Claude OAuth Messages behavior: identity/cache/adaptive thinking.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the compiler consumes the resolved descriptor into the effective configuration"
)]
pub fn compile_messages_oauth_configuration(
    selected: &SelectedProviderConfiguration,
    descriptor: ResolvedModelDescriptor,
    authority: ResolvedProviderAuthorityBinding,
    api_version: &str,
    beta_header: &str,
) -> Result<CompiledProviderConfiguration, MessagesProviderConfigurationError> {
    let behavior = selected.behavior();
    let exact = matches!(
        behavior.processing_tier,
        plakous_model::ProcessingTierDisposition::Standard
    ) && matches!(
        behavior.reasoning_summary,
        ReasoningSummaryDisposition::Omit
    ) && matches!(
        behavior.reasoning_include,
        ReasoningIncludeDisposition::Omit
    ) && matches!(behavior.replay, ReplayDisposition::StrictEncrypted)
        && matches!(
            behavior.cache,
            CachePolicy::Enabled { .. } | CachePolicy::Disabled
        )
        && matches!(
            behavior.parallel_tools,
            ParallelToolDisposition::Enabled | ParallelToolDisposition::Disabled
        )
        && matches!(behavior.output.format(), OutputFormat::Text)
        && matches!(
            behavior.storage,
            ProviderStorageDisposition::OmittedRouteContract
        )
        && matches!(behavior.response_state, ResponseStateDisposition::Stateless)
        && matches!(
            behavior.local_durability,
            LocalDurabilityDisposition::Ephemeral
                | LocalDurabilityDisposition::BestEffort
                | LocalDurabilityDisposition::DurableRequired
        )
        && matches!(
            behavior.retention,
            RetentionDisposition::RouteContract { .. }
        )
        && !matches!(
            behavior.tool_choice,
            ToolChoice::Required | ToolChoice::Specific(_)
        )
        && behavior
            .tools
            .iter()
            .all(|tool| tool.execution() == ToolExecutionDisposition::Local)
        && api_version == "2023-06-01"
        && beta_header.contains("oauth-2025-04-20")
        && beta_header.contains("claude-code-20250219");
    if !exact {
        return Err(MessagesProviderConfigurationError::PolicyMismatch);
    }
    let family = descriptor
        .model_family()
        .cloned()
        .ok_or(MessagesProviderConfigurationError::PolicyMismatch)?;
    let thinking = match behavior.reasoning_effort {
        ReasoningEffortDisposition::Disabled => "disabled",
        ReasoningEffortDisposition::Enabled(_) => "adaptive",
    };
    let cache = match behavior.cache {
        CachePolicy::Enabled { .. } => "ephemeral",
        _ => "disabled",
    };
    let bytes = serde_json::to_vec(&Policy {
        api_version,
        beta_header,
        stream: true,
        thinking,
        cache,
        storage: "stateless-omitted-route-contract",
        aggregate_continuation: "absent",
        managed_cache_artifact: "absent",
        native_compaction: "absent",
        item_replay_schema_revision: crate::MESSAGES_REPLAY_SCHEMA_REVISION,
    })
    .map_err(|_| MessagesProviderConfigurationError::Identity)?;
    let policy = FamilyPolicyEnvelope::new(
        family,
        MESSAGES_FAMILY_POLICY_SCHEMA_REVISION,
        MESSAGES_FAMILY_POLICY_COMPILER_REVISION,
        MESSAGES_REQUEST_POLICY_REVISION,
        bytes,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        selected,
        descriptor.clone(),
        plakous_model::ModelImplementationIdentity::release_owned(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            &descriptor,
            1,
        )?,
        authority,
        behavior.clone(),
        policy,
    )?;
    Ok(CompiledProviderConfiguration::new(
        selected.clone(),
        effective,
    )?)
}

/// Messages configuration compilation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessagesProviderConfigurationError {
    /// The explicit selection differs from the conservative policy.
    PolicyMismatch,
    /// Canonical policy identity failed.
    Identity,
    /// Neutral compilation rejected resolved facts.
    Provider(ProviderConfigurationError),
}

impl From<ProviderConfigurationError> for MessagesProviderConfigurationError {
    fn from(value: ProviderConfigurationError) -> Self {
        Self::Provider(value)
    }
}

impl std::fmt::Display for MessagesProviderConfigurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PolicyMismatch => {
                f.write_str("selected behavior does not match the conservative Messages policy")
            }
            Self::Identity => f.write_str("Messages policy identity failed"),
            Self::Provider(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for MessagesProviderConfigurationError {}

#[cfg(test)]
mod tests;
