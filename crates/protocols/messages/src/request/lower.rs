use std::collections::BTreeSet;

use plakous_attempt::ReplaySafety;
use plakous_model::{
    ActiveFamilyArtifact, Actor, CachePolicy, CapabilityRequirement, Digest, DigestDomain,
    HostedTool, InstructionSource, OutputFormat, ReasoningPolicy, ToolCallerDisposition,
    ToolChoice, ToolInputContract, ToolName, ToolOwnerProjectionError, ValidatedRequestPlan,
};
use serde_json::Value;

use super::{
    CLAUDE_OAUTH_CONTEXT_MANAGEMENT, DiagnosticsParam, LoweredMessagesRequest,
    MESSAGES_REPLAY_SCHEMA_REVISION, MessageRole, MessagesRequestBody, MessagesRequestError,
    MessagesRequestPolicy, MetadataParam, RouterParam, SerializedBodyTransform, ServerToolParam,
    SystemBlock, ThinkingParam, ToolDefinitionParam, ToolOwnership, ToolParam, invalid,
    mark_last_message_cache_breakpoint, push_message, text_block, translate_assistant_bound,
    translate_history, translate_reasoning, translate_tool_choice, unsupported,
};

/// Lowers one already provider-validated canonical plan into Messages wire bytes.
#[allow(
    clippy::too_many_lines,
    reason = "one fail-closed lowering boundary keeps semantic checks ordered"
)]
pub fn lower_request(
    plan: &ValidatedRequestPlan,
    policy: &MessagesRequestPolicy,
    ownership: ToolOwnership,
) -> Result<LoweredMessagesRequest, MessagesRequestError> {
    lower_request_with_active_artifact(plan, policy, ownership, None)
}

/// Lowers one request while validating and consuming the sole active Messages artifact.
#[allow(
    clippy::too_many_lines,
    reason = "one fail-closed lowering boundary keeps semantic and artifact checks ordered"
)]
pub fn lower_request_with_active_artifact(
    plan: &ValidatedRequestPlan,
    policy: &MessagesRequestPolicy,
    mut ownership: ToolOwnership,
    active_artifact: Option<&ActiveFamilyArtifact>,
) -> Result<LoweredMessagesRequest, MessagesRequestError> {
    let request = plan.request();
    if plan.binding_digest() != policy.descriptor_binding
        || request.selected_provider_configuration_id() != policy.selected_provider_configuration
        || request.effective_provider_configuration_id() != policy.effective_provider_configuration
    {
        return Err(invalid(
            "Messages request authority does not match the exact admitted target",
        ));
    }
    if !request.native().entries().is_empty() {
        return Err(unsupported(
            "native options are not enabled by this Messages profile",
        ));
    }
    if !matches!(request.output().format(), OutputFormat::Text) {
        return Err(unsupported(
            "structured output is outside the admitted Messages profile",
        ));
    }
    let prompt_cache = match request.cache() {
        CachePolicy::Disabled | CachePolicy::Enabled { .. } if policy.prompt_cache => true,
        CachePolicy::Disabled => false,
        CachePolicy::Enabled { requirement } => {
            return Err(match requirement {
                CapabilityRequirement::Required => {
                    unsupported("Messages prompt-cache writes are not admitted by this profile")
                }
                _ => unsupported(
                    "preferred Messages prompt cache cannot be silently downgraded during lowering",
                ),
            });
        }
        _ => {
            return Err(unsupported(
                "unknown Messages cache policy cannot be translated",
            ));
        }
    };
    let mut system = Vec::new();
    for identity in &policy.system_prefixes {
        system.push(SystemBlock {
            kind: "text",
            text: identity.clone(),
            cache_control: None,
        });
    }
    for item in request.instructions() {
        system.push(SystemBlock {
            kind: "text",
            text: item.text().to_owned(),
            cache_control: None,
        });
    }
    if prompt_cache {
        let skip_identity_only =
            !policy.system_prefixes.is_empty() && system.len() == policy.system_prefixes.len();
        let cache_index = if skip_identity_only || system.is_empty() {
            None
        } else {
            Some(system.len() - 1)
        };
        if let Some(index) = cache_index {
            system[index].cache_control = Some(policy.cache_control_param());
        }
    }
    let programmatic = request
        .hosted_tools()
        .contains(&HostedTool::ProgrammaticComposition);
    if request
        .hosted_tools()
        .contains(&HostedTool::CodeInterpreter)
    {
        return Err(unsupported(
            "Messages does not admit code-interpreter hosted tools",
        ));
    }
    let caller_policy = match (programmatic, policy.programmatic_tool_caller.as_ref()) {
        (true, Some(policy)) => Some(policy),
        (false, None) => None,
        _ => {
            return Err(invalid(
                "Messages hosted-tool selection does not match the exact policy",
            ));
        }
    };
    if programmatic && request.tool_projection().is_some() {
        return Err(unsupported(
            "Messages hosted programmatic tools require exact configured tool names",
        ));
    }
    let mut tools = if let Some(projection) = request.tool_projection() {
        projection
            .definitions()
            .iter()
            .map(|entry| {
                let Some(input_schema) = entry.input_schema() else {
                    return Err(unsupported(
                        "Messages does not admit projected free-form local tools",
                    ));
                };
                Ok(ToolParam::Customer(ToolDefinitionParam {
                    name: entry.exposed_name().as_str().to_owned(),
                    description: entry.description().to_owned(),
                    input_schema: input_schema.wire_document(),
                    cache_control: None,
                    allowed_callers: None,
                }))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        request
            .tools()
            .iter()
            .map(|tool| {
                let definition = tool.definition();
                let ToolInputContract::Structured(schema) = definition.input() else {
                    return Err(unsupported("Messages does not admit free-form local tools"));
                };
                let allowed_callers = caller_policy.and_then(|policy| match tool.caller() {
                    ToolCallerDisposition::DirectOnly => None,
                    ToolCallerDisposition::ProviderProgrammaticOnly => {
                        Some(policy.request_callers.clone())
                    }
                    ToolCallerDisposition::DirectAndProviderProgrammatic => {
                        let mut callers = Vec::with_capacity(policy.request_callers.len() + 1);
                        callers.push("direct".into());
                        callers.extend(policy.request_callers.iter().cloned());
                        Some(callers)
                    }
                });
                Ok(ToolParam::Customer(ToolDefinitionParam {
                    name: definition.name().as_str().to_owned(),
                    description: definition.description().to_owned(),
                    input_schema: schema.wire_document(),
                    cache_control: None,
                    allowed_callers,
                }))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    if prompt_cache && let Some(ToolParam::Customer(tool)) = tools.last_mut() {
        tool.cache_control = Some(policy.cache_control_param());
    }
    if let Some(policy) = caller_policy {
        tools.push(ToolParam::Server(ServerToolParam {
            kind: &policy.response_caller,
            name: "code_execution",
        }));
    }
    let advertised: BTreeSet<ToolName> = if let Some(projection) = request.tool_projection() {
        projection
            .definitions()
            .iter()
            .map(|entry| entry.exposed_name().clone())
            .collect()
    } else {
        request
            .tools()
            .iter()
            .map(|tool| tool.definition().name().clone())
            .collect()
    };
    if ownership
        .owners
        .keys()
        .any(|name| !advertised.contains(name))
    {
        return Err(invalid(
            "tool ownership named a tool absent from the validated request",
        ));
    }
    for name in advertised {
        ownership
            .owners
            .entry(name.clone())
            .or_insert_with(|| request.local_tool_owner(&name));
        let caller = request
            .tools()
            .iter()
            .find(|tool| tool.definition().name() == &name)
            .map_or(
                ToolCallerDisposition::DirectOnly,
                plakous_model::ConfiguredTool::caller,
            );
        ownership.callers.insert(name, caller);
    }
    for tool in request.tools() {
        let owner = ownership.owner(tool.definition().name());
        tool.validate_owner_projection(&owner)
            .map_err(|error| match error {
                ToolOwnerProjectionError::OwnerMismatch => invalid(error.to_string()),
            })?;
    }
    let mut replay_diagnostics = Vec::new();
    // Semantic compact (and any other Disabled-reasoning request) must not
    // replay thinking blocks: Anthropic 400s when `thinking` is disabled.
    let include_native_thinking = !matches!(request.reasoning(), ReasoningPolicy::Disabled);
    let (mut messages, mut container, historical_projection_diagnostics) = translate_history(
        request.input(),
        policy,
        &ownership,
        programmatic,
        include_native_thinking,
        &mut replay_diagnostics,
    )?;
    // Semantic compact's sealed prefix ends on a complete assistant group.
    // Anthropic Messages requires the last role to be user (prefill is not used).
    if messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Assistant)
    {
        let trailing = request
            .instructions()
            .iter()
            .rev()
            .find(|item| item.source() == InstructionSource::Harness)
            .map(plakous_model::InstructionBlock::text)
            .filter(|text| !text.is_empty())
            .map_or_else(
                || plakous_model::SEMANTIC_COMPACTION_INSTRUCTION.to_owned(),
                str::to_owned,
            );
        push_message(&mut messages, MessageRole::User, vec![text_block(trailing)]);
    }
    if prompt_cache {
        mark_last_message_cache_breakpoint(&mut messages, policy.cache_control_param());
    }
    let tool_set = messages_tool_set_identity(request, policy, &ownership)?;
    let configuration = messages_artifact_configuration_identity(policy, plan.digest())?;
    let artifact_binding = messages_artifact_binding(policy, tool_set, configuration)?;
    let mut active_artifact_lineage = None;
    if let Some(active) = active_artifact {
        if !programmatic {
            return Err(invalid(
                "active Messages artifact requires hosted programmatic composition",
            ));
        }
        let artifact =
            crate::artifact::MessagesFamilyArtifact::from_active(active, artifact_binding)
                .map_err(|error| invalid(error.to_string()))?;
        let covered_index = usize::try_from(artifact.covered_items)
            .map_err(|_| invalid("Messages artifact covered-item count overflow"))?;
        let assistant = covered_index
            .checked_sub(1)
            .and_then(|index| request.input().items().get(index))
            .filter(|item| item.actor() == Actor::Assistant)
            .ok_or_else(|| invalid("Messages artifact does not cover one assistant output"))?;
        let mut replay_container = None;
        let mut lineage = artifact.prior_lineage();
        let (native, _) = translate_assistant_bound(
            assistant.blocks(),
            policy,
            &ownership,
            programmatic,
            include_native_thinking,
            &mut replay_container,
            &mut lineage,
            &mut replay_diagnostics,
        )?;
        artifact
            .validate_reuse(
                plan,
                tool_set,
                configuration,
                &native,
                replay_container
                    .as_deref()
                    .or(Some(artifact.container.as_str())),
            )
            .map_err(|error| invalid(error.to_string()))?;
        let replay_message = messages
            .iter_mut()
            .rfind(|message| {
                message.role == MessageRole::Assistant
                    && crate::artifact::native_blocks_match_canonical(
                        artifact.native_blocks(),
                        &message.content,
                    )
            })
            .ok_or_else(|| invalid("Messages artifact assistant replay was not found"))?;
        replay_message.content = artifact.native_blocks().to_vec();
        if container
            .as_deref()
            .is_some_and(|value| value != artifact.container)
        {
            return Err(invalid(
                "Messages artifact container drifted during complete lowering",
            ));
        }
        container = Some(artifact.container.clone());
        active_artifact_lineage = Some(artifact.active_lineage());
    }
    let configured_max_tokens = request
        .limits()
        .max_output_tokens()
        .map_or(policy.maximum_output_tokens, std::num::NonZeroU32::get);
    if configured_max_tokens > policy.maximum_output_tokens {
        return Err(invalid(
            "requested max output tokens exceed the exact model limit",
        ));
    }
    let max_tokens = policy
        .request_output_cap
        .map_or(configured_max_tokens, |cap| configured_max_tokens.min(cap));
    if max_tokens == 0 {
        return Err(invalid("Messages max_tokens must be non-zero"));
    }
    let (thinking, output_config) = translate_reasoning(
        request.reasoning(),
        policy.minimum_thinking_budget,
        max_tokens,
        policy.omit_disabled_thinking,
        policy.thinking_display,
    )?;
    if matches!(&thinking, Some(ThinkingParam::Enabled { .. }))
        && matches!(
            request.tool_choice(),
            ToolChoice::Required | ToolChoice::Specific(_)
        )
    {
        return Err(invalid(
            "manual Messages thinking is incompatible with forced or specific tool choice",
        ));
    }
    let active_artifact_container = active_artifact_lineage
        .as_ref()
        .and_then(|_| container.clone());
    let has_tools = !tools.is_empty();
    let body = MessagesRequestBody {
        model: policy.requested_model.as_str(),
        max_tokens,
        cache_control: (prompt_cache && policy.top_level_prompt_cache)
            .then(|| policy.cache_control_param()),
        system,
        messages,
        container,
        tools,
        tool_choice: translate_tool_choice(request.tool_choice(), has_tools)?,
        thinking,
        context_management: policy
            .claude_oauth_context_management
            .then_some(CLAUDE_OAUTH_CONTEXT_MANAGEMENT),
        output_config,
        provider: policy.router.as_ref().map(RouterParam::gateway_managed),
        metadata: policy
            .metadata_user_id
            .as_deref()
            .map(|user_id| MetadataParam { user_id }),
        stop_sequences: request.limits().stop_sequences(),
        plugins: policy.plugins,
        session_id: policy.session_id.as_deref(),
        diagnostics: policy.claude_oauth_diagnostics.then_some(DiagnosticsParam {
            previous_message_id: policy
                .claude_oauth_previous_message_id
                .as_ref()
                .map(|id| id.0.as_str()),
        }),
        stream: true,
    };
    let mut body = serde_json::to_vec(&body)
        .map_err(|e| invalid(format!("failed to serialize Messages request: {e}")))?;
    if let Some(SerializedBodyTransform(transform)) = policy.serialized_body_transform {
        transform(&mut body).map_err(invalid)?;
    }
    let owner_bytes = serde_json::to_vec(&ownership)
        .map_err(|e| invalid(format!("failed to bind tool ownership: {e}")))?;
    let router_identity = messages_router_identity(policy)?;
    let operation_digest = Digest::sha256(
        DigestDomain::RequestPlan,
        3,
        &[
            &plan.digest().identity_bytes(),
            &plan.binding_digest().identity_bytes(),
            policy.api_version.as_bytes(),
            policy.beta_header.as_bytes(),
            &policy.account_binding.identity_bytes(),
            &policy
                .selected_provider_configuration
                .digest()
                .identity_bytes(),
            &policy
                .effective_provider_configuration
                .digest()
                .identity_bytes(),
            &MESSAGES_REPLAY_SCHEMA_REVISION.to_be_bytes(),
            &router_identity,
            &owner_bytes,
            &body,
        ],
    )
    .map_err(|e| invalid(format!("failed to digest Messages request: {e}")))?;
    replay_diagnostics.sort_unstable();
    replay_diagnostics.dedup();
    let replay_safety = classify_replay_safety(&body);
    Ok(LoweredMessagesRequest {
        replay_safety,
        body,
        max_tokens,
        operation_digest,
        tool_ownership: ownership,
        active_artifact_lineage,
        active_artifact_container,
        replay_diagnostics,
        historical_projection_diagnostics,
    })
}

/// Top-level Messages fields whose replay can only cost duplicate inference.
/// `context_management` edits transform what the request itself sent.
const COST_ONLY_FIELDS: &[&str] = &[
    "cache_control",
    "context_management",
    "diagnostics",
    "max_tokens",
    "messages",
    "metadata",
    "model",
    "output_config",
    "plugins",
    "provider",
    "session_id",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "thinking",
    "tool_choice",
    "tools",
    "top_k",
    "top_p",
];

/// Classifies the exact serialized body by allowlist: only client tools
/// (no `type`, or `custom`) without programmatic callers, disabled gateway
/// plugins, and the fields above are cost-only. A server tool, a container,
/// an enabled plugin, or any field nobody classified is a server effect.
fn classify_replay_safety(body: &[u8]) -> ReplaySafety {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return ReplaySafety::ServerEffect;
    };
    let pure = fields.iter().all(|(name, value)| match name.as_str() {
        "tools" => value.as_array().is_some_and(|tools| {
            tools.iter().all(|tool| {
                tool.as_object().is_some_and(|tool| {
                    !tool.contains_key("allowed_callers")
                        && tool
                            .get("type")
                            .is_none_or(|kind| kind.as_str() == Some("custom"))
                })
            })
        }),
        "plugins" => value.as_array().is_some_and(|plugins| {
            plugins
                .iter()
                .all(|plugin| plugin.get("enabled") == Some(&Value::Bool(false)))
        }),
        name => COST_ONLY_FIELDS.contains(&name),
    });
    if pure {
        ReplaySafety::CostOnly
    } else {
        ReplaySafety::ServerEffect
    }
}

pub(crate) fn messages_tool_set_identity(
    request: &plakous_model::ModelRequest,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
) -> Result<Digest, MessagesRequestError> {
    let tools = serde_json::to_vec(&(request.tools(), request.hosted_tools(), ownership))
        .map_err(|_| invalid("Messages tool set could not be sealed"))?;
    let caller_policy =
        serde_json::to_vec(&policy.programmatic_tool_caller.as_ref().map(|caller| {
            (
                &caller.request_callers,
                &caller.response_caller,
                &caller.namespace,
            )
        }))
        .map_err(|_| invalid("Messages caller policy could not be sealed"))?;
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-messages/hosted-tool-set-v1",
            &tools,
            &caller_policy,
        ],
    )
    .map_err(|_| invalid("Messages tool set identity failed"))
}

pub(crate) fn messages_artifact_configuration_identity(
    policy: &MessagesRequestPolicy,
    _plan: Digest,
) -> Result<Digest, MessagesRequestError> {
    let router_identity = messages_router_identity(policy)?;
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-messages/hosted-configuration-v1",
            &policy
                .selected_provider_configuration
                .digest()
                .identity_bytes(),
            &policy
                .effective_provider_configuration
                .digest()
                .identity_bytes(),
            policy.api_version.as_bytes(),
            policy.beta_header.as_bytes(),
            &router_identity,
        ],
    )
    .map_err(|_| invalid("Messages configuration identity failed"))
}

fn messages_router_identity(
    policy: &MessagesRequestPolicy,
) -> Result<Vec<u8>, MessagesRequestError> {
    policy.router.as_ref().map_or_else(
        || Ok(Vec::new()),
        |router| {
            serde_json::to_vec(&RouterParam::gateway_managed(router))
                .map_err(|_| invalid("Messages router identity failed"))
        },
    )
}

pub(crate) fn messages_artifact_binding(
    policy: &MessagesRequestPolicy,
    tool_set: Digest,
    configuration: Digest,
) -> Result<Digest, MessagesRequestError> {
    Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[
            b"plakous-messages/hosted-artifact-binding-v1",
            &policy.descriptor_binding.identity_bytes(),
            &policy.account_binding.identity_bytes(),
            &tool_set.identity_bytes(),
            &configuration.identity_bytes(),
            policy.requested_model.as_str().as_bytes(),
        ],
    )
    .map_err(|_| invalid("Messages artifact binding identity failed"))
}
