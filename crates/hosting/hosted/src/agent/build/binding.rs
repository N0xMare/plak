//! Runtime bindings and provider configuration.

use crate::{CliFailure, CliResult};

use super::policy::{
    NATIVE_RUN_MODEL_CLEANUP_TIMEOUT, NATIVE_RUN_MODEL_OPERATION_TIMEOUT, RESPONSE_OUTPUT_CEILING,
    SessionToolFence, composition_is_js_ptc, composition_is_py_ptc,
    composition_requests_native_ptc, install_occupancy_fence, observation_budget_cap_bytes,
    product_javascript_candidate_attachment, product_python_attachment, session_tool_policy,
    stamped_workspace_policy,
};
use super::profile::{enroll_product_instructions, selected_harness_profile};
use super::tools::{
    NodeSession, bind_node_tool_plan, coding_tool_registry, coding_tool_registry_with_extensions,
    direct_composition_capable, exact_process_presentation_candidate, is_codex_code_mode_candidate,
    is_exact_code_mode_attachment, node_session, projection_target,
};
use super::topology::{
    ApiTopologyParent, ProductAgentSpec, SpawnParent, topology_extensions_if_enabled,
    topology_extensions_if_enabled_with_owner,
};
use crate::binding::CompositionSelection;
use plakous::agent::{
    AgentBuilder, AgentConfig, AgentRequestPolicy, CapabilityRequirement, GenerationLimits,
    ReasoningEffort, ReasoningPolicy, compile_explicit_runtime_profile,
};
use plakous::model::{
    CachePolicy, ConfiguredTool, ContextAdmissionRequirement, LocalDurabilityDisposition, ModelId,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProcessingTierDisposition,
    ProviderBehaviorConfiguration, ProviderStorageDisposition, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningSummaryDisposition, ReplayDisposition,
    ResponseStateDisposition, RetentionDisposition, SelectedProviderConfiguration, ToolChoice,
    ToolExecutionDisposition, ToolPresentationMode,
};
use plakous::sessions::{SessionRuntimeBinding, SessionTargetBinding, isolated_tool_scheduler};
use plakous::subscriptions::{
    NativeProvider, NativeToolMode, ResolvedProvider, native_provider_route_selection,
};
use plakous::tools::{
    CodexMinimalLocalCodeModeV1, CompositionRuntime, ProductJavaScriptCompositionV1,
    PythonPtcPresentationV1, PythonPtcV1, plan_restoration_note,
};
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Materializes Plak's complete explicit provider selection before any live
/// model resolution occurs.
pub fn selected_coding_provider_configuration(
    provider: NativeProvider,
    model: ModelId,
    effort: impl Into<Option<ReasoningEffort>>,
    native_tool_mode: Option<NativeToolMode>,
    composition: CompositionSelection,
) -> CliResult<SelectedProviderConfiguration> {
    selected_coding_provider_configuration_with_processing_tier(
        provider,
        model,
        effort.into(),
        native_tool_mode,
        composition,
        ProcessingTierDisposition::Standard,
    )
}

/// Materializes an exact provider selection with an independently selected
/// processing tier. Product admission must validate named tiers against the
/// authenticated catalog before calling this constructor.
pub fn selected_coding_provider_configuration_with_processing_tier(
    provider: NativeProvider,
    model: ModelId,
    effort: Option<ReasoningEffort>,
    native_tool_mode: Option<NativeToolMode>,
    composition: CompositionSelection,
    processing_tier: ProcessingTierDisposition,
) -> CliResult<SelectedProviderConfiguration> {
    if provider == NativeProvider::Codex && effort == Some(ReasoningEffort::Ultra) {
        return Err(CliFailure::new(
            "Codex Ultra is a multi-agent orchestration mode, not a single-agent reasoning effort",
        ));
    }
    let candidate_enrolled = provider.enrolls(model.as_str());
    if !candidate_enrolled {
        return Err(CliFailure::new(
            "model has no exact built-in process-presentation candidate",
        ));
    }
    let route = native_provider_route_selection(provider, model.clone())
        .map_err(|_| CliFailure::new("native provider route is invalid"))?;
    let code_mode_candidate = match (provider, native_tool_mode, composition) {
        (_, _, CompositionSelection::NativePtc)
            if !matches!(
                (provider, native_tool_mode),
                (NativeProvider::Codex, Some(NativeToolMode::CodeModeOnly))
            ) =>
        {
            return Err(CliFailure::new(crate::binding::native_ptc_refusal(
                provider,
            )));
        }
        (NativeProvider::Codex, Some(NativeToolMode::Direct), _) => false,
        (
            NativeProvider::Codex,
            Some(NativeToolMode::CodeModeOnly),
            CompositionSelection::NativePtc | CompositionSelection::FollowCatalog,
        ) => {
            if !is_codex_code_mode_candidate(provider, route.requested_model()) {
                return Err(CliFailure::new(
                    "Codex model requires CodeModeOnly but has no exact enrolled local composition presentation",
                ));
            }
            true
        }
        // explicit Direct on a CodeModeOnly row is a second enrolled
        // presentation (no `exec`/`wait`), and Plak js/py-ptc may replace that
        // Direct presentation exactly as on a catalog-Direct row. Plak PTC never
        // attaches *beside* Code Mode because this arm compiles no Code Mode.
        (
            NativeProvider::Codex,
            Some(NativeToolMode::CodeModeOnly),
            CompositionSelection::Direct
            | CompositionSelection::JsPtc
            | CompositionSelection::PyPtc,
        ) => false,
        (NativeProvider::Codex, Some(NativeToolMode::CodeMode), _) => {
            return Err(CliFailure::new(
                "Codex hybrid CodeMode is catalog-admitted but has no exact enrolled product presentation",
            ));
        }
        (NativeProvider::Codex, None, _) => {
            return Err(CliFailure::new(
                "Codex model has no recognized authenticated tool-mode authority",
            ));
        }
        (_, None, _) => false,
        (_, Some(_), _) => {
            return Err(CliFailure::new(
                "native tool-mode evidence belongs to a different provider family",
            ));
        }
    };
    let product_javascript = composition_is_js_ptc(composition);
    let product_python = composition_is_py_ptc(composition);
    if (product_javascript || product_python)
        && !direct_composition_capable(provider, native_tool_mode)
    {
        return Err(CliFailure::new(
            "Plak-owned custom composition requires exact Direct-capable catalog authority",
        ));
    }
    let registry = coding_tool_registry("provider-selection", composition, code_mode_candidate)?;
    let catalog = crate::coding_routes::selected_route_presentation(provider, model.as_str())?;
    let outer_selection = if product_javascript || product_python {
        composition
    } else if code_mode_candidate {
        CompositionSelection::NativePtc
    } else {
        CompositionSelection::Direct
    };
    let selected_names = outer_selection
        .outer_marker_names()
        .unwrap_or_else(|| catalog.configured_canonical_names());
    let mut tools = selected_names
        .iter()
        .map(|name| {
            let definition = registry
                .definitions()
                .iter()
                .find(|definition| definition.name().as_str() == *name)
                .cloned()
                .ok_or_else(|| CliFailure::new("coding catalog capability is not registered"))?;
            Ok(ConfiguredTool::new(
                definition,
                ToolExecutionDisposition::Local,
                plakous::model::ToolCallerDisposition::DirectOnly,
            ))
        })
        .collect::<CliResult<Vec<_>>>()?;
    tools.sort_by(|left, right| left.definition().name().cmp(right.definition().name()));
    let (reasoning_summary, reasoning_include, replay, parallel_tools, output_bound, storage) =
        match provider {
            // Codex asks for `reasoning.summary: "auto"` like the official
            // client whenever an effort is on the wire; without an effort the
            // request carries no reasoning object and so no summary either
            // (the dialect's behaviour check expects exactly that pairing).
            NativeProvider::Codex => (
                if effort.is_some() {
                    ReasoningSummaryDisposition::Auto
                } else {
                    ReasoningSummaryDisposition::Omit
                },
                ReasoningIncludeDisposition::EncryptedContent,
                ReplayDisposition::StrictEncrypted,
                // Every release-enrolled CodeModeOnly row in the reviewed
                // 0.162.1 reference catalog uses Responses Lite, whose descriptor does
                // not advertise ParallelTools. A future catalog change may
                // expose it, but disabling parallel calls remains safe.
                if native_tool_mode == Some(NativeToolMode::CodeModeOnly) {
                    ParallelToolDisposition::Disabled
                } else {
                    ParallelToolDisposition::Enabled
                },
                OutputBoundDisposition::NoRequestBound,
                ProviderStorageDisposition::DisabledOnWire,
            ),
            NativeProvider::Xai => (
                ReasoningSummaryDisposition::Concise,
                ReasoningIncludeDisposition::EncryptedContent,
                ReplayDisposition::StrictEncrypted,
                ParallelToolDisposition::Disabled,
                OutputBoundDisposition::Enforced(
                    plakous::xai::grok_max_output_tokens(model.as_str())
                        .map(|ceiling| ceiling.min(RESPONSE_OUTPUT_CEILING))
                        .and_then(NonZeroU32::new)
                        .ok_or(CliFailure::new("Grok model has no enrolled output ceiling"))?,
                ),
                ProviderStorageDisposition::DisabledOnWire,
            ),
            NativeProvider::Claude => (
                ReasoningSummaryDisposition::Omit,
                ReasoningIncludeDisposition::Omit,
                ReplayDisposition::StrictEncrypted,
                ParallelToolDisposition::Enabled,
                OutputBoundDisposition::Enforced(
                    NonZeroU32::new(plakous::anthropic::claude_max_output_tokens(model.as_str()))
                        .ok_or(CliFailure::new("Claude output limit is invalid"))?,
                ),
                ProviderStorageDisposition::OmittedRouteContract,
            ),
            _ => return Err(CliFailure::new("native coding provider is unsupported")),
        };
    let retention = RetentionDisposition::RouteContract {
        profile: route.integration_profile().clone(),
        revision: route.integration_revision().clone(),
    };
    SelectedProviderConfiguration::new(
        route,
        ProviderBehaviorConfiguration {
            processing_tier,
            // Provider projection and harness-local composition are separate
            // axes. Native Codex Code Mode is a free-form outer composition
            // surface; Plak-owned JavaScript/Python projections are ordinary
            // structured function tools on a Direct-capable provider route.
            tool_mode: if code_mode_candidate {
                ToolPresentationMode::LocalCompositionOnly
            } else {
                ToolPresentationMode::Direct
            },
            reasoning_effort: effort.map_or(
                ReasoningEffortDisposition::Disabled,
                ReasoningEffortDisposition::Enabled,
            ),
            reasoning_summary,
            reasoning_include,
            replay,
            cache: match provider {
                NativeProvider::Claude => CachePolicy::Enabled {
                    requirement: CapabilityRequirement::Required,
                },
                NativeProvider::Codex | NativeProvider::Xai | _ => CachePolicy::Disabled,
            },
            hosted_tools: Vec::new(),
            tools,
            tool_choice: ToolChoice::Auto,
            parallel_tools,
            output: OutputPolicy::default(),
            output_bound,
            storage,
            retention,
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::DurableRequired,
        },
    )
    .map_err(|_| CliFailure::new("native provider behavior is invalid"))
}

pub(crate) struct PreparedApiRuntime {
    pub runtime: SessionRuntimeBinding,
    pub evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
    /// Set when this reopen kept a budget the route has since moved.
    pub output_bound_rederived: Option<crate::openrouter::OutputBoundRederived>,
}

/// Compiles against an attached hub without acquiring session ownership.
pub(crate) async fn api_session_runtime_binding(
    parent: &ApiTopologyParent,
    opening_durable_session: bool,
    tool_fence: SessionToolFence,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
) -> CliResult<PreparedApiRuntime> {
    api_session_runtime_binding_with_owner(
        parent,
        opening_durable_session,
        tool_fence,
        crate::topology_runtime::TopologyHubAttach::LookupOnly,
        cached_evidence,
        &mut None,
    )
    .await
}

/// An API runtime whose topology hub is looked up, or attached when a
/// profile fork's own tree has none yet. The caller drops an attached lease.
pub(crate) async fn api_session_runtime_binding_for(
    parent: &ApiTopologyParent,
    opening_durable_session: bool,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    acquired_owner: &mut Option<String>,
) -> CliResult<PreparedApiRuntime> {
    api_session_runtime_binding_with_owner(
        parent,
        opening_durable_session,
        SessionToolFence::Full,
        topology_attach,
        None,
        acquired_owner,
    )
    .await
}

pub(super) async fn api_session_runtime_binding_with_owner(
    parent: &ApiTopologyParent,
    opening_durable_session: bool,
    tool_fence: SessionToolFence,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
    acquired_owner: &mut Option<String>,
) -> CliResult<PreparedApiRuntime> {
    let tool_plan = bind_node_tool_plan(
        &parent.tool_selection,
        Some(crate::tool_plan::ToolRoute::api(parent.target)),
        parent.target.account(),
        &parent.workspace_root,
        &parent.state_roots,
    )?;
    let spawn_parent = SpawnParent::Api(parent.clone());
    let topology_extensions = topology_extensions_if_enabled_with_owner(
        &spawn_parent,
        &parent.model,
        topology_attach,
        opening_durable_session,
        acquired_owner,
    )?
    .with_plan(&tool_plan)?;
    install_occupancy_fence(&parent.cwd)?;
    match parent.target {
        crate::binding::ApiTarget::OpenRouter { wire } => {
            let prepared = Box::pin(crate::openrouter::construct_openrouter_runtime_with_fence(
                crate::openrouter::ProductOpenRouterSpec {
                    job_config: parent.state_roots.job_config(),
                    exchange_observer: parent.state_roots.exchange_observer(),
                    credentials: parent.state_roots.openrouter_credentials(),
                    sidecars: Arc::clone(parent.state_roots.sidecars()),
                    product_home: parent.product_home.clone(),
                    literal_model: parent.model.clone(),
                    effort: parent.effort,
                    composition: parent.composition,
                    wire,
                    workspace_root: parent.workspace_root.clone(),
                    cwd: parent.cwd.clone(),
                    plan_state_authority: parent.plan_state_authority.clone(),
                    opening_durable_session,
                    occupancy_tier: crate::agent_tree_protocol::Tier::Mutate,
                    scheduled_compaction: parent.scheduled_compaction,
                    max_model_rounds: parent.max_model_rounds,
                    topology_extensions,
                    cached_evidence,
                    telemetry: parent.state_roots.telemetry_emitter(),
                    spawned_child: parent.topology_caller_id.is_some(),
                    created_output_bound: parent.created_output_bound,
                },
                tool_fence,
            ))
            .await?;
            Ok(PreparedApiRuntime {
                runtime: prepared.runtime.with_job_host(
                    parent.state_roots.job_registry().for_agent(
                        parent
                            .topology_caller_id
                            .clone()
                            .unwrap_or_else(|| "root".to_owned()),
                        parent
                            .topology_tree_id
                            .as_deref()
                            .unwrap_or(parent.plan_state_authority.as_str()),
                    ),
                ),
                output_bound_rederived: prepared.output_bound_rederived,
                evidence: Some(prepared.evidence),
            })
        }
        crate::binding::ApiTarget::Platform { provider } => {
            let telemetry = parent.state_roots.telemetry_emitter();
            let admission = parent.state_roots.platform_admission();
            let runtime = Box::pin(crate::platform::construct_platform_runtime_with(
                provider,
                parent.state_roots.platform_credentials(),
                &crate::api_runtime::ApiRuntimeSpec {
                    job_config: parent.state_roots.job_config(),
                    account: provider.account(),
                    sidecars: parent.state_roots.sidecars().as_ref(),
                    product_home: parent.product_home.as_deref(),
                    literal_model: &parent.model,
                    effort: parent.effort,
                    composition: parent.composition,
                    wire: provider.protocol(),
                    workspace_root: &parent.workspace_root,
                    cwd: &parent.cwd,
                    plan_state_authority: &parent.plan_state_authority,
                    opening_durable_session,
                    tool_fence,
                    scheduled_compaction: parent.scheduled_compaction,
                    max_model_rounds: parent.max_model_rounds,
                    topology_extensions: &topology_extensions,
                    telemetry: telemetry.as_ref(),
                    spawned_child: parent.topology_caller_id.is_some(),
                },
                parent.state_roots.exchange_observer(),
                &admission,
                Arc::new(plakous::model::SystemClock),
            ))
            .await?;
            Ok(PreparedApiRuntime {
                output_bound_rederived: None,
                runtime: runtime.with_job_host(
                    parent.state_roots.job_registry().for_agent(
                        parent
                            .topology_caller_id
                            .clone()
                            .unwrap_or_else(|| "root".to_owned()),
                        parent
                            .topology_tree_id
                            .as_deref()
                            .unwrap_or(parent.plan_state_authority.as_str()),
                    ),
                ),
                evidence: None,
            })
        }
    }
}

/// Constructs the exact sealed runtime authority used by create, open, and
/// driver-owned session workflows.
pub fn coding_session_runtime_binding(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
) -> CliResult<SessionRuntimeBinding> {
    coding_session_runtime_binding_for(
        spec,
        stderr,
        false,
        SessionToolFence::Full,
        crate::topology_runtime::TopologyHubAttach::LookupOnly,
        &mut None,
    )
}

/// Fresh conversation create (`/clear`) must attach a new root tree, not look
/// up the previous hub lease.
pub fn coding_session_runtime_binding_attaching_root(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
) -> CliResult<SessionRuntimeBinding> {
    coding_session_runtime_binding_for(
        spec,
        stderr,
        false,
        SessionToolFence::Full,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        &mut None,
    )
}

/// Builds a native runtime with the caller's attach choice and reports the
/// tree id when that build took an `AttachRoot` lease.
pub(crate) fn coding_session_runtime_binding_with_attach(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
    attach: crate::topology_runtime::TopologyHubAttach,
    attached_tree: &mut Option<String>,
) -> CliResult<SessionRuntimeBinding> {
    coding_session_runtime_binding_for(
        spec,
        stderr,
        false,
        SessionToolFence::Full,
        attach,
        attached_tree,
    )
}

pub(in crate::agent) const fn native_provider_kind(provider: NativeProvider) -> &'static str {
    match provider {
        NativeProvider::Codex => "codex",
        NativeProvider::Xai => "xai",
        NativeProvider::Claude => "claude",
        _ => "native",
    }
}

pub(in crate::agent) fn coding_session_runtime_binding_for(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
    opening_durable_session: bool,
    fence: SessionToolFence,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    attached_tree: &mut Option<String>,
) -> CliResult<SessionRuntimeBinding> {
    let topology_spec = spec.clone();
    let ProductAgentSpec {
        product_home,
        prompt_host_facts: _,
        provider,
        resolved,
        effort,
        effort_explicit,
        workspace_root,
        cwd,
        plan_state_authority,
        composition,
        topology_tree_id: _,
        topology_caller_id: _,
        state_roots,
        scheduled_compaction,
        max_model_rounds,
        topology_enabled: _,
        tool_selection,
    } = spec;
    let _binding_span = tracing::info_span!(
        "session_runtime_binding",
        provider = native_provider_kind(provider),
        model = resolved.selected_model().as_str(),
        composition = composition.as_str(),
        session_id = plan_state_authority.as_str(),
    )
    .entered();
    let model = resolved.model();

    let resolved_tool_mode = resolved.native_tool_mode();
    let code_mode_candidate = composition_requests_native_ptc(composition)
        && is_exact_code_mode_attachment(provider, resolved.selected_model(), resolved_tool_mode);
    let product_javascript = composition_is_js_ptc(composition);
    let product_python = composition_is_py_ptc(composition);
    if composition == CompositionSelection::NativePtc && !code_mode_candidate {
        return Err(CliFailure::new(crate::binding::native_ptc_refusal(
            provider,
        )));
    }
    if (product_javascript || product_python)
        && !direct_composition_capable(provider, resolved_tool_mode)
    {
        return Err(CliFailure::new(
            "Plak-owned custom composition requires exact Direct-capable catalog authority; no inference was dispatched",
        ));
    }
    if product_javascript || product_python {
        if resolved.configuration().selected().behavior().tool_mode != ToolPresentationMode::Direct
            || resolved.configuration().effective().behavior().tool_mode
                != ToolPresentationMode::Direct
        {
            return Err(CliFailure::new(
                "product composition provider projection is not Direct-capable",
            ));
        }
    } else if code_mode_candidate {
        resolved
            .require_native_tool_mode(NativeToolMode::CodeModeOnly)
            .map_err(|error| {
                CliFailure::new(format!(
                    "Codex Code Mode requires exact CodeModeOnly catalog authority: {error}"
                ))
            })?;
        if resolved.configuration().selected().behavior().tool_mode
            != ToolPresentationMode::LocalCompositionOnly
            || resolved.configuration().effective().behavior().tool_mode
                != ToolPresentationMode::LocalCompositionOnly
        {
            return Err(CliFailure::new(
                "Codex Code Mode provider configuration has the wrong exact tool mode",
            ));
        }
    } else if provider == NativeProvider::Codex {
        match resolved_tool_mode {
            Some(NativeToolMode::Direct)
                if resolved.configuration().effective().behavior().tool_mode
                    == ToolPresentationMode::Direct => {}
            Some(NativeToolMode::CodeModeOnly)
                if resolved.configuration().effective().behavior().tool_mode
                    == ToolPresentationMode::Direct => {}
            Some(NativeToolMode::CodeModeOnly) => {
                return Err(CliFailure::new(
                    "Codex catalog requires CodeModeOnly for a model without the exact V1 presentation",
                ));
            }
            Some(NativeToolMode::CodeMode) => {
                return Err(CliFailure::new(
                    "Codex hybrid CodeMode has no exact enrolled product presentation",
                ));
            }
            Some(NativeToolMode::Direct) => {
                return Err(CliFailure::new(
                    "Codex Direct catalog authority resolved to a non-Direct presentation",
                ));
            }
            None => {
                return Err(CliFailure::new(
                    "Codex model has no recognized authenticated tool-mode authority",
                ));
            }
        }
    }

    let tool_plan = bind_node_tool_plan(
        &tool_selection,
        crate::tool_plan::ToolRoute::native(provider),
        state_roots.roster_source().account_name(provider),
        &workspace_root,
        &state_roots,
    )?;
    let product_extensions = topology_extensions_if_enabled(
        &SpawnParent::from_native(topology_spec.clone()),
        resolved.selected_model().as_str(),
        topology_attach,
        opening_durable_session,
        attached_tree,
    )?
    .with_plan(&tool_plan)?;
    install_occupancy_fence(&cwd)?;
    product_extensions.connect_retrieval(resolved.retrieval_port())?;
    product_extensions.connect_image_gen(resolved.image_gen_port())?;
    let frozen_tools = coding_tool_registry_with_extensions(
        &plan_state_authority,
        composition,
        code_mode_candidate,
        &product_extensions,
    )?;
    let extra_nested = product_extensions.names().cloned().collect::<Vec<_>>();
    let direct_extras =
        !extra_nested.is_empty() && !product_javascript && !product_python && !code_mode_candidate;
    // One callable surface for this node, compiled once with the session's
    // own configuration: the presentation's core menu plus every extension.
    // It is the nested catalog the PTC descriptors list, the names the PTC
    // base instruction names, and the set the earned agreement paragraphs are
    // judged against.
    let runtime = if product_javascript {
        Some(CompositionRuntime::ProductJavaScript)
    } else if product_python {
        Some(CompositionRuntime::PythonPtc)
    } else if code_mode_candidate {
        Some(CompositionRuntime::CodexNative)
    } else {
        None
    };
    let NodeSession {
        configured_model,
        configuration: session_configuration,
        presentation: presentation_kind,
        surface,
    } = node_session(
        state_roots.job_config(),
        resolved.configured_model(),
        &frozen_tools,
        &product_extensions,
        if direct_extras { &extra_nested } else { &[] },
        fence,
        runtime,
    )?;
    // A fresh resolution past the admitted window is compiled for this node
    // exactly as the construction above compiled the current one.
    let model_reresolver = {
        let (job_config, registry, extensions) = (
            state_roots.job_config(),
            frozen_tools.clone(),
            product_extensions.clone(),
        );
        let extras = if direct_extras {
            extra_nested.clone()
        } else {
            Vec::new()
        };
        crate::agent::reresolve::NativeModelReresolver::for_resolution(
            provider,
            &resolved,
            Box::new(move |configured| {
                node_session(
                    job_config,
                    configured,
                    &registry,
                    &extensions,
                    &extras,
                    fence,
                    runtime,
                )
                .map(|session| session.configured_model)
            }),
        )
    };
    let effective_tools = &session_configuration.effective().behavior().tools;
    if effective_tools.iter().any(|configured| {
        frozen_tools
            .definitions()
            .iter()
            .all(|actual| configured.definition() != actual)
    }) {
        return Err(CliFailure::new(
            "resolved provider tool configuration does not match the product registry",
        ));
    }
    let physical_root = std::fs::canonicalize(&workspace_root)
        .map_err(|_| CliFailure::new("recorded workspace root could not be resolved"))?;
    ensure_workspace_excludes_product_state(&physical_root, state_roots.paths())?;
    let physical_cwd = std::fs::canonicalize(&cwd)
        .map_err(|_| CliFailure::new("workspace current directory could not be resolved"))?;
    let relative_cwd = physical_cwd.strip_prefix(&physical_root).map_err(|_| {
        CliFailure::new("workspace current directory is outside the recorded workspace root")
    })?;
    let workspace = Arc::new(stamped_workspace_policy(&physical_root, relative_cwd)?);
    let request_output_bound = resolved
        .configuration()
        .effective()
        .behavior()
        .output_bound
        .bound();
    let generation_limits = GenerationLimits::new(request_output_bound, Vec::new())
        .map_err(|_| CliFailure::new("native run generation policy is invalid"))?;
    // Required-or-omit: harness plan PreferEffort omits reasoning
    // when the model does not admit it; when enabled, require it honestly.
    let reasoning = match effort {
        Some(effort) => ReasoningPolicy::Enabled {
            effort: Some(effort),
            requirement: CapabilityRequirement::Required,
        },
        None => ReasoningPolicy::Disabled,
    };
    let request_policy = AgentRequestPolicy::new(generation_limits, reasoning);
    let config = AgentConfig::default()
        .with_observation_budget_cap_bytes(observation_budget_cap_bytes(
            resolved
                .configuration()
                .effective()
                .descriptor()
                .context_contract()
                .map_or(0, |contract| contract.capacity().rule().input_ceiling()),
        ))
        .with_max_model_rounds(max_model_rounds)
        .with_model_cleanup_timeout(NATIVE_RUN_MODEL_CLEANUP_TIMEOUT)
        .with_model_operation_timeout(NATIVE_RUN_MODEL_OPERATION_TIMEOUT)
        // Full-tool multi-step coding emits more live events than smoke.
        .with_event_capacity(4_096)
        .with_event_byte_capacity(16 * 1024 * 1024)
        .with_scheduled_compaction_enabled(scheduled_compaction);

    let builder = AgentBuilder::new(
        configured_model,
        frozen_tools.clone(),
        Arc::clone(&workspace),
    )
    // The product compiled the projection below from its own pack list and
    // route catalog, so the product is what admits it at construction.
    .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
    .post_compaction_note(Arc::new(plan_restoration_note))
    .host_process_options(crate::process_environment::options(&physical_root)?)
    .job_config(state_roots.job_config())
    .job_host(
        state_roots.job_registry().for_agent(
            topology_spec
                .topology_caller_id
                .clone()
                .unwrap_or_else(|| "root".to_owned()),
            topology_spec
                .topology_tree_id
                .as_deref()
                .unwrap_or(topology_spec.plan_state_authority.as_str()),
        ),
    );
    let mut builder = builder
        .tool_policy(session_tool_policy(fence))
        .request_slots(crate::request_slots::for_account(
            state_roots.roster_source().account_name(provider),
        ));
    if let Some((reresolver, admitted_until)) = model_reresolver {
        builder = builder.model_reresolver(reresolver, admitted_until);
    }
    if let Some(telemetry) = state_roots.telemetry_emitter() {
        builder = builder.telemetry(telemetry);
    }

    if product_javascript {
        let target = projection_target(resolved.configuration(), "product JavaScript target")?;
        let descriptor = ProductJavaScriptCompositionV1::compile_for_kind(
            state_roots.job_config(),
            frozen_tools.clone(),
            target,
            presentation_kind,
            surface.exposed_names().iter().cloned(),
        )
        .map_err(|error| CliFailure::new(format!(
            "product JavaScript descriptor could not be compiled: {error}; no inference was dispatched"
        )))?;
        let host_path = state_roots.sidecars().javascript_host().map_err(|error| {
            CliFailure::new(format!(
                "product JavaScript host is unavailable: {error}; set PLAK_JS_HOST or place plakous-js-host beside plak; no inference was dispatched"
            ))
        })?;
        let attachment = product_javascript_candidate_attachment(
            &descriptor,
            &host_path,
            resolved.configuration(),
            presentation_kind,
        )
        .ok_or_else(|| {
            CliFailure::new(
                "product JavaScript artifact or evidence is unavailable; no inference was dispatched",
            )
        })?;
        builder = plakous::agent::attach_local_composition(builder, Some(attachment)).map_err(
            |error| {
                CliFailure::new(format!(
                    "product JavaScript artifact or evidence is unavailable: {error}; no inference was dispatched"
                ))
            },
        )?;
    } else if product_python {
        let target = projection_target(resolved.configuration(), "product Python target")?;
        let descriptor = PythonPtcV1::compile_for_kind(
            state_roots.job_config(),
            frozen_tools.clone(),
            target,
            PythonPtcPresentationV1::StructuredFunctions,
            presentation_kind,
            surface.exposed_names().iter().cloned(),
        )
        .map_err(|error| {
            CliFailure::new(format!(
                "product Python descriptor could not be compiled: {error}; no inference was dispatched"
            ))
        })?;
        let host_path = state_roots.sidecars().python_host().map_err(|error| {
            CliFailure::new(format!(
                "product Python host is unavailable: {error}; set PLAK_PY_HOST or place plakous-py-host beside plak; no inference was dispatched"
            ))
        })?;
        let attachment = product_python_attachment(
            &descriptor,
            &host_path,
            resolved.configuration(),
            presentation_kind,
        )?;
        builder = plakous::agent::attach_local_composition(builder, Some(attachment)).map_err(
            |error| {
                CliFailure::new(format!(
                    "product Python artifact or evidence is unavailable: {error}; no inference was dispatched"
                ))
            },
        )?;
    } else if code_mode_candidate {
        let target = projection_target(resolved.configuration(), "Code Mode resolved target")?;
        let descriptor = CodexMinimalLocalCodeModeV1::compile_for_catalog(
            state_roots.job_config(),
            frozen_tools.clone(),
            target,
            &surface
                .exposed_names()
                .iter()
                .filter(|name| name.as_str() != "inspect_image")
                .cloned()
                .collect::<Vec<_>>(),
        )
        .map_err(|error| {
            CliFailure::new(format!(
                "Code Mode descriptor could not be compiled: {error}"
            ))
        })?;
        let host_path = state_roots.sidecars().javascript_host().map_err(|error| {
            CliFailure::new(format!(
                "Codex Code Mode sidecar is unavailable: {error}; set PLAK_JS_HOST or place plakous-js-host beside plak"
            ))
        })?;
        builder =
            plakous::agent::codex_minimal_local_code_mode_sidecar(builder, &descriptor, host_path)
                .map_err(|error| {
                    CliFailure::new(format!(
                        "Codex Code Mode (rquickjs sidecar) could not attach: {error}"
                    ))
                })?;
    } else if !opening_durable_session {
        let projection = exact_process_presentation_candidate(
            state_roots.job_config(),
            provider,
            resolved.as_ref(),
            &session_configuration,
            frozen_tools.clone(),
            if direct_extras {
                extra_nested.as_slice()
            } else {
                &[]
            },
        )?;
        builder = builder.tool_projection(if fence == SessionToolFence::Full {
            projection
        } else {
            surface.projection().clone()
        });
    }

    // Exact reopen must not offer a freshly compiled product default as a
    // replacement for the reconstruction-complete plan in the checkpoint.
    // Plakous rehydrates that plan and still validates its model, tools,
    // workspace, provider configuration, and fresh runtime authority.
    // Rotate `.subagents.v1` only when topology extras actually compiled
    // (Direct extras or Code Mode nested names), not merely because the
    // product flag is on.
    let profile = if opening_durable_session {
        None
    } else {
        selected_harness_profile(
            provider,
            resolved.selected_model(),
            effort,
            opening_durable_session,
            code_mode_candidate,
            extra_nested.as_slice(),
            !extra_nested.is_empty(),
            composition,
            &surface,
            workspace.root(),
            workspace.cwd(),
        )?
    };
    if let Some(profile) = profile {
        let (profile, context_sources) = enroll_product_instructions(
            &profile,
            product_home.as_deref(),
            workspace.root(),
            workspace.cwd(),
        )?;
        // Explicit harness plans own reasoning at agent construction and would
        // otherwise overwrite CLI `--effort` with the profile default (Codex
        // High / Grok Low). Pin operator effort into the plan before compile.
        let plan = compile_explicit_runtime_profile(
            &profile,
            model.as_ref(),
            &session_configuration,
            &frozen_tools,
            workspace.as_ref(),
            context_sources,
        )
        .map_err(|error| CliFailure::new(format!("harness plan could not be compiled: {error}")))?;
        // Effort / preferred-feature downgrades must not stay silent when
        // the operator chose `--effort`, and should at least warn otherwise.
        let effort_downgraded = plan
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.as_str().contains("reasoning effort downgraded"));
        if effort_downgraded {
            for diagnostic in plan.diagnostics() {
                let _ = writeln!(stderr, "plan diagnostic: {}", diagnostic.as_str());
            }
            if effort_explicit {
                return Err(CliFailure::new(
                    "requested --effort was downgraded by the harness plan; refuse to run",
                ));
            }
        }
        builder = builder.harness_plan(plan);
    }

    if topology_spec.topology_caller_id.is_some() {
        builder = isolated_tool_scheduler(builder).map_err(|error| {
            CliFailure::new(format!(
                "spawn child tool scheduler is invalid ({:?}): {error}",
                error.kind()
            ))
        })?;
    }

    Ok(SessionRuntimeBinding::new(
        builder.config(config).request_policy(request_policy),
    ))
}

pub(in crate::agent) fn ensure_workspace_excludes_product_state(
    workspace: &Path,
    protected: impl IntoIterator<Item = PathBuf>,
) -> CliResult<()> {
    if protected
        .into_iter()
        .any(|path| path.starts_with(workspace) || workspace.starts_with(&path))
    {
        return Err(CliFailure::new(
            "workspace overlaps Plak credential or product state authority",
        ));
    }
    Ok(())
}

/// Exact compaction runtime: same coding binding as the live turn with compact max output only.
pub fn compaction_session_runtime_binding(
    spec: &ProductAgentSpec,
) -> CliResult<SessionRuntimeBinding> {
    let mut sink = std::io::sink();
    let binding = coding_session_runtime_binding(spec.clone(), &mut sink)?;
    binding
        .with_compaction_output_limit()
        .map_err(|_| CliFailure::new("compaction generation policy is invalid"))
}

#[must_use]
pub fn coding_session_target_binding(resolved: &ResolvedProvider) -> SessionTargetBinding {
    let route = resolved.configuration().selected().route();
    SessionTargetBinding::new(
        route.protocol().clone(),
        route.route().clone(),
        route.requested_model().clone(),
        resolved.configuration().selected_identity(),
        resolved.configuration().effective_identity(),
        resolved.initial_authority().account_binding(),
    )
}
