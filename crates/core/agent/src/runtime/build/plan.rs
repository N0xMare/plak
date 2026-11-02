use super::{
    AgentBuilder, AgentError, AgentErrorKind, AgentRequestPolicy, CachePolicy,
    CapabilityRequirement, CompiledProviderConfiguration, FrozenToolRegistry, GenerationLimits,
    InstructionBlock, NativeOptions, ProfileError, ReasoningEffortDisposition, ReasoningPolicy,
    ResolvedHarnessPlan, StableRequestPrefix, ToolExecutionDisposition, ToolExposure,
    ToolExposureMap, ToolRuntime, compile_construction_bound_plan, compiler_model_facts_from_model,
    profile_construction_error, stable_workspace_environment_digest,
};

pub(in crate::runtime) fn resolve_construction_harness_plan(
    builder: &AgentBuilder,
    tools: &ToolRuntime,
) -> Result<
    (
        ResolvedHarnessPlan,
        AgentRequestPolicy,
        Vec<InstructionBlock>,
        ToolExposureMap,
    ),
    AgentError,
> {
    let registry_digest = tools.registry_identity_digest().map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("frozen tool registry could not be digested: {error}"),
        )
    })?;
    let canonical_tool_names = tools
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str().to_owned())
        .collect::<Vec<_>>();
    // Plan compilation must use run-stable workspace facts, never the
    // run-scoped execution-environment allocation identity. Recovery reopens
    // mint a new RunId and would otherwise invalidate plan digests.
    let environment_digest = stable_workspace_environment_digest(
        builder.workspace.as_ref(),
        tools.environment().descriptor().backend(),
    )?;

    let plan = if let Some(plan) = builder.harness_plan.as_ref() {
        let model_facts = compiler_model_facts_from_model(
            builder.model.as_ref(),
            &builder.provider_configuration,
        )
        .map_err(|error| profile_construction_error(&error))?;
        plan.ensure_model_binding(&model_facts)
            .map_err(|error| profile_construction_error(&error))?;
        plan.clone()
    } else {
        compile_construction_bound_plan(
            builder.model.as_ref(),
            &builder.provider_configuration,
            registry_digest,
            canonical_tool_names,
            environment_digest,
            &builder.instructions,
            builder.request_policy.reasoning(),
            true,
        )
        .map_err(|error| profile_construction_error(&error))?
    };
    if plan.tool_registry_digest() != registry_digest
        || plan.execution_environment_digest() != environment_digest
    {
        let change = if plan.tool_registry_digest() == registry_digest {
            crate::BindingChange::ExecutionEnvironment
        } else {
            crate::BindingChange::ToolRegistry
        };
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("resolved harness plan is bound to a different {change}"),
        )
        .with_binding_change(change));
    }
    let derived_exposure = derive_plan_tool_exposure(&builder.tools, &plan)?;
    let tool_exposure_map = derived_exposure;

    // Explicit plans own model-facing instruction/reasoning decisions.
    // Construction-bound plans record those decisions for identity only and
    // must not rewrite the caller's exact request policy or instruction prefix.
    if builder.harness_plan.is_some() {
        let request_policy = builder
            .request_policy
            .clone()
            .with_reasoning(plan.reasoning_policy());
        let instructions = plan
            .instruction_blocks()
            .map_err(|error| profile_construction_error(&error))?;
        Ok((plan, request_policy, instructions, tool_exposure_map))
    } else {
        Ok((
            plan,
            builder.request_policy.clone(),
            builder.instructions.clone(),
            tool_exposure_map,
        ))
    }
}

fn effective_request_policy(
    configuration: &CompiledProviderConfiguration,
) -> Result<AgentRequestPolicy, AgentError> {
    let behavior = configuration.effective().behavior();
    let reasoning = match behavior.reasoning_effort {
        ReasoningEffortDisposition::Disabled => ReasoningPolicy::Disabled,
        ReasoningEffortDisposition::Enabled(effort) => ReasoningPolicy::Enabled {
            effort: Some(effort),
            requirement: CapabilityRequirement::Required,
        },
    };
    let max_output_tokens = behavior.output_bound.bound();
    let limits = GenerationLimits::new(max_output_tokens, Vec::new()).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective output bound could not be represented in the model request",
        )
    })?;
    Ok(AgentRequestPolicy::new(limits, reasoning)
        .with_context_admission_requirement(behavior.context_evidence_requirement))
}

pub(in crate::runtime) fn stable_prefix_from_effective_configuration(
    configuration: &CompiledProviderConfiguration,
    request_policy: &AgentRequestPolicy,
    instructions: Vec<InstructionBlock>,
    exposure: &ToolExposureMap,
    harness_plan: &ResolvedHarnessPlan,
) -> Result<StableRequestPrefix, AgentError> {
    let behavior = configuration.effective().behavior();
    let expected_policy = effective_request_policy(configuration)?;
    let configured_tools = behavior.tools.clone();
    let expected_cache = if harness_plan.cache().prefer_stable_prefix() {
        CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        }
    } else {
        CachePolicy::Disabled
    };
    if request_policy != &expected_policy {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective provider behavior disagrees with the agent request policy",
        ));
    }
    if (!exposure.definitions().is_empty()
        && !configured_tools
            .iter()
            .map(plakous_model::ConfiguredTool::definition)
            .eq(exposure.definitions()))
        || behavior
            .tools
            .iter()
            .any(|tool| tool.execution() != ToolExecutionDisposition::Local)
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective provider tool behavior disagrees with the agent tool exposure",
        ));
    }
    if behavior.cache != expected_cache {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "effective provider cache behavior disagrees with the harness plan",
        ));
    }
    StableRequestPrefix::new(
        configuration.selected_identity(),
        configuration.effective_identity(),
        behavior.processing_tier.clone(),
        instructions,
        configured_tools,
        behavior.hosted_tools.clone(),
        behavior.output.clone(),
        expected_policy.reasoning(),
        behavior.tool_choice.clone(),
        behavior.cache,
        expected_policy.generation_limits().clone(),
        NativeOptions::default(),
        Some(*harness_plan.plan_digest()),
    )
    .and_then(|prefix| {
        prefix.with_context_admission_requirement(expected_policy.context_admission_requirement())
    })
    .map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("stable model-request prefix is invalid: {error}"),
        )
    })
}

fn derive_plan_tool_exposure(
    registry: &FrozenToolRegistry,
    plan: &ResolvedHarnessPlan,
) -> Result<ToolExposureMap, AgentError> {
    let planned = plan.tools().exposed_names();
    if !planned.is_empty() && planned.len() != registry.definitions().len() {
        return Err(profile_construction_error(
            &ProfileError::PlanToolExposureMismatch,
        ));
    }
    if planned.is_empty() {
        return ToolExposureMap::new(registry.clone(), []).map_err(|error| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                format!("resolved tool exposure map is invalid: {error}"),
            )
        });
    }
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "resolved plan references a tool without executable identity",
                    )
                })?;
            let override_entry = plan
                .tools()
                .overrides()
                .iter()
                .rev()
                .find(|entry| entry.canonical_name() == definition.name().as_str());
            let exposed_name = override_entry
                .map_or_else(|| definition.name().as_str(), |entry| entry.exposed_name());
            ToolExposure::new(identity, exposed_name.to_owned())
                .and_then(|exposure| {
                    exposure.with_description_override(
                        override_entry
                            .and_then(|entry| entry.description())
                            .map(str::to_owned),
                    )
                })
                .map_err(|error| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        format!("resolved tool exposure is invalid: {error}"),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut derived_names = exposures
        .iter()
        .map(|exposure| exposure.exposed_name().as_str())
        .collect::<Vec<_>>();
    let mut planned_names = planned.iter().map(String::as_str).collect::<Vec<_>>();
    derived_names.sort_unstable();
    planned_names.sort_unstable();
    if derived_names != planned_names {
        return Err(profile_construction_error(
            &ProfileError::PlanToolExposureMismatch,
        ));
    }
    ToolExposureMap::new(registry.clone(), exposures).map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("resolved tool exposure map is invalid: {error}"),
        )
    })
}
