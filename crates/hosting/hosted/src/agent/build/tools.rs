//! Tool registries, plans and presentation choices.

use crate::{CliFailure, CliResult};

use super::policy::SessionToolFence;
use crate::agent::ProductStateRoots;
use crate::binding::CompositionSelection;
use crate::tools::{ProductToolExtensions, product_surface_with_fence};
use plakous::model::{
    CompiledProviderConfiguration, ConfiguredLanguageModel, ConfiguredTool,
    EffectiveProviderConfiguration, ModelId, ProcessingTierDisposition,
    SelectedProviderConfiguration, ToolExecutionDisposition,
};
use plakous::subscriptions::{
    NativeProvider, NativeToolMode, ResolvedProvider, native_provider_route_selection,
};
use plakous::tools::{
    CodexNativeLocalCodeModeV1, CodingPresentation, CompiledSurface, CompositionRuntime,
    FrozenToolRegistry, InMemoryPlanState, PROCESS_TOOL_PROJECTION_REVISION, PlanStateIdentity,
    ProcessSessionTools, ProductJavaScriptCompositionV1, ProviderModelProjectionIdentityV2,
    PythonHarnessSdkV1, PythonPtcV1, ToolProjectionMap, ToolRegistryBuilder,
    process_presentation_candidate_with_extensions_v2,
};
use std::path::Path;
use std::sync::Arc;

pub(crate) fn coding_tool_registry(
    plan_state_authority: &str,
    composition: CompositionSelection,
    attach_native: bool,
) -> CliResult<FrozenToolRegistry> {
    coding_tool_registry_with_extensions(
        plan_state_authority,
        composition,
        attach_native,
        &ProductToolExtensions::empty(),
    )
}

pub(crate) fn coding_tool_registry_with_extensions(
    plan_state_authority: &str,
    composition: CompositionSelection,
    attach_native: bool,
    extensions: &ProductToolExtensions,
) -> CliResult<FrozenToolRegistry> {
    let identity = PlanStateIdentity::new(format!("plak.session.{plan_state_authority}"), 1)
        .map_err(|_| CliFailure::new("plan-state authority identity is invalid"))?;
    let process = Arc::new(
        ProcessSessionTools::posix_sh()
            .map_err(|_| CliFailure::new("process-session tools could not be initialized"))?
            .with_config(extensions.process_config()),
    );
    // Every executor is a pack's own registration: the four coding packs and
    // the product extension pack. Composition markers below are enrollment for
    // a driver, not a model-visible tool, and are not a pack.
    let mut builder = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(
            Arc::new(InMemoryPlanState::with_identity(identity)),
            Arc::clone(&process),
        ),
        &mut builder,
    )
    .map_err(|_| CliFailure::new("coding tools could not be initialized"))?;
    extensions.register_into(&mut builder)?;
    match composition {
        CompositionSelection::Direct => {}
        CompositionSelection::NativePtc if attach_native => {
            CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)
                .map_err(|_| CliFailure::new("Code Mode markers could not be initialized"))?;
        }
        CompositionSelection::FollowCatalog if attach_native => {
            CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)
                .map_err(|_| CliFailure::new("Code Mode markers could not be initialized"))?;
        }
        CompositionSelection::JsPtc => {
            ProductJavaScriptCompositionV1::register_markers_into(&mut builder).map_err(|_| {
                CliFailure::new("product JavaScript markers could not be initialized")
            })?;
        }
        CompositionSelection::PyPtc => {
            PythonPtcV1::register_markers_into(&mut builder)
                .map_err(|_| CliFailure::new("product Python markers could not be initialized"))?;
        }
        CompositionSelection::PySdk => {
            PythonHarnessSdkV1::register_into(&mut builder, process).map_err(|error| {
                CliFailure::new(format!("Python SDK registration is invalid: {error}"))
            })?;
        }
        CompositionSelection::FollowCatalog | CompositionSelection::NativePtc => {}
    }
    Ok(builder.freeze())
}

/// The projection identity of one resolved target; `subject` names the
/// caller in the two failure messages.
pub(crate) fn projection_target(
    configuration: &CompiledProviderConfiguration,
    subject: &str,
) -> CliResult<ProviderModelProjectionIdentityV2> {
    let effective = configuration.effective();
    let descriptor = effective.descriptor();
    let family = descriptor
        .model_family()
        .ok_or_else(|| CliFailure::new(format!("{subject} has no exact model family")))?;
    ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        family.as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|_| CliFailure::new(format!("{subject} identity is invalid")))
}

/// The model and provider configuration a node's session is constructed with,
/// and its callable surface compiled for exactly that configuration.
pub(in crate::agent) struct NodeSession {
    pub(in crate::agent) configured_model: ConfiguredLanguageModel,
    pub(in crate::agent) configuration: CompiledProviderConfiguration,
    pub(in crate::agent) presentation: CodingPresentation,
    pub(in crate::agent) surface: CompiledSurface,
}

/// Compiles a node's session configuration and its callable surface together.
///
/// Direct topology extras (`direct_extras`, empty when the presentation does
/// not carry them as Direct tools) are part of the session configuration's
/// effective identity, so a tier-filtered projection compiled for the
/// unextended configuration names a target the projection admission refuses.
/// Returning both from one place keeps that pairing out of the caller's hands.
pub(in crate::agent) fn node_session(
    job_config: plakous::tools::JobConfig,
    resolved: ConfiguredLanguageModel,
    registry: &FrozenToolRegistry,
    extensions: &ProductToolExtensions,
    direct_extras: &[plakous::model::ToolName],
    fence: SessionToolFence,
    runtime: Option<CompositionRuntime>,
) -> CliResult<NodeSession> {
    let configuration = compiled_configuration_with_direct_extensions(
        resolved.configuration(),
        registry,
        direct_extras,
    )?;
    let configured_model = if direct_extras.is_empty() {
        resolved
    } else {
        ConfiguredLanguageModel::from_registered(
            resolved.registered().clone(),
            configuration.clone(),
            resolved.initial_authority().clone(),
        )
        .map_err(|_| CliFailure::new("extended Direct model configuration is invalid"))?
    };
    let presentation = CodingPresentation::admit(
        &configuration,
        crate::coding_routes::product_presentation_routes()?,
    )
    .map_err(|_| CliFailure::new("native provider is unsupported"))?;
    let surface = product_surface_with_fence(
        job_config,
        presentation,
        registry.clone(),
        projection_target(&configuration, "product tool surface target")?,
        extensions,
        fence,
        runtime,
    )?;
    Ok(NodeSession {
        configured_model,
        configuration,
        presentation,
        surface,
    })
}

pub(in crate::agent) fn is_codex_code_mode_candidate(
    provider: NativeProvider,
    model: &ModelId,
) -> bool {
    provider == NativeProvider::Codex
        && provider.enrolled_tool_mode(model.as_str()) == Some(NativeToolMode::CodeModeOnly)
}

/// Frozen processing tier for this child's template: a child runs on its own
/// card's tier, never its parent's. A node whose card is not in the current
/// epoch is Standard.
pub(in crate::agent) fn spawn_roster_processing_tier(
    tree_id: &str,
    agent_id: &str,
) -> ProcessingTierDisposition {
    crate::topology_runtime::lookup(tree_id)
        .and_then(|runtime| {
            runtime
                .read(|tree| {
                    let template = tree.node(agent_id).ok()?.template.clone()?;
                    Some(tree.snapshot.binding(&template)?.processing_tier.clone())
                })
                .ok()
                .flatten()
        })
        .unwrap_or(ProcessingTierDisposition::Standard)
}

/// A spawned template carries its own composition: a template
/// that omits `composition` resolves to plakous-default for that template's
/// own account/model, never to the parent's presentation. `None` here means
/// the child has no template binding at all, not "the template said nothing".
pub(in crate::agent) fn spawn_roster_composition(
    tree_id: &str,
    agent_id: &str,
) -> Option<CompositionSelection> {
    crate::topology_runtime::lookup(tree_id).and_then(|runtime| {
        runtime
            .read(|tree| {
                let node = tree.node(agent_id).ok()?;
                let template = node.template.clone()?;
                let binding = tree.snapshot.binding(&template)?;
                Some(binding.composition.map_or(
                    CompositionSelection::FollowCatalog,
                    CompositionSelection::from,
                ))
            })
            .ok()
            .flatten()
    })
}

/// Roster pin is bind authority. A child's composition comes from
/// its own template or, unpinned, from the hub. Plak js-ptc/py-ptc
/// is inherited by every unpinned child, Codex included: on a CodeModeOnly row
/// it replaces the explicit Direct presentation, as on spark.
/// Plakous-default and native-ptc are provider-bound: native-ptc is the
/// vertical's own PTC system and only Codex enrolls one (Code Mode), so both
/// are inherited only by a Codex child, and a native-ptc root can still spawn
/// a Claude or Grok class — it resolves its own presentation through
/// plakous-default instead of refusing.
pub fn spawn_child_composition(
    parent_composition: CompositionSelection,
    target_provider: NativeProvider,
    roster_composition: Option<CompositionSelection>,
) -> Result<CompositionSelection, String> {
    let child_composition = match roster_composition {
        Some(pinned) => pinned,
        None => match (parent_composition, target_provider) {
            (CompositionSelection::FollowCatalog, NativeProvider::Codex) => parent_composition,
            (CompositionSelection::FollowCatalog, _) => CompositionSelection::Direct,
            (CompositionSelection::NativePtc, NativeProvider::Codex) => parent_composition,
            (CompositionSelection::NativePtc, _) => CompositionSelection::FollowCatalog,
            _ => parent_composition,
        },
    };
    if child_composition == CompositionSelection::PySdk {
        return Err("spawn child: py-sdk is root-only and has no child result channel".into());
    }
    if child_composition == CompositionSelection::NativePtc
        && target_provider != NativeProvider::Codex
    {
        return Err(format!(
            "spawn child: {}; or drop `composition = \"native-ptc\"` from its [agent] template \
             so it resolves its own presentation",
            crate::binding::native_ptc_refusal(target_provider)
        ));
    }
    Ok(child_composition)
}

pub(in crate::agent) fn is_exact_code_mode_attachment(
    provider: NativeProvider,
    model: &ModelId,
    native_tool_mode: Option<NativeToolMode>,
) -> bool {
    native_tool_mode == Some(NativeToolMode::CodeModeOnly)
        && is_codex_code_mode_candidate(provider, model)
}

/// Whether this row can carry the Direct coding pack, and therefore a Plak
/// js/py-ptc overlay on it. Codex `Direct` rows are Direct by catalog;
/// `CodeModeOnly` rows are Direct-capable through the explicit second
/// presentation. Hybrid `CodeMode` stays unenrolled.
pub fn direct_composition_capable(
    provider: NativeProvider,
    native_tool_mode: Option<NativeToolMode>,
) -> bool {
    match provider {
        NativeProvider::Codex => matches!(
            native_tool_mode,
            Some(NativeToolMode::Direct | NativeToolMode::CodeModeOnly)
        ),
        NativeProvider::Xai | NativeProvider::Claude => native_tool_mode.is_none(),
        _ => false,
    }
}

pub(in crate::agent) fn exact_process_presentation_candidate(
    job_config: plakous::tools::JobConfig,
    provider: NativeProvider,
    resolved: &ResolvedProvider,
    configuration: &CompiledProviderConfiguration,
    registry: FrozenToolRegistry,
    extra_nested: &[plakous::model::ToolName],
) -> CliResult<ToolProjectionMap> {
    let route = configuration.selected().route();
    let requested = route.requested_model();
    let exact_route = native_provider_route_selection(provider, requested.clone())
        .map_err(|_| CliFailure::new("native process presentation candidate route is invalid"))?;
    if &exact_route != route || resolved.selected_model() != requested {
        return Err(CliFailure::new(
            "native process presentation candidate does not match the exact selected route",
        ));
    }
    let candidate_enrolled = provider.enrolls(requested.as_str());
    if !candidate_enrolled {
        return Err(CliFailure::new(
            "model has no exact built-in process-presentation candidate",
        ));
    }
    process_presentation_candidate_with_extensions_v2(
        job_config,
        registry,
        configuration,
        crate::coding_routes::product_presentation_routes()?,
        extra_nested,
    )
    .map_err(|error| {
        CliFailure::new(format!(
            "native process presentation candidate could not be compiled: {error}"
        ))
    })
}

/// Rebuilds selected/effective tools so Direct extras are prefix-seal covered.
///
/// Children inherit `parent.resolved` and apply grant-compiled extras at bind.
/// Topology names are never stored on the resolved provider.
pub(in crate::agent) fn compiled_configuration_with_direct_extensions(
    source: &CompiledProviderConfiguration,
    registry: &FrozenToolRegistry,
    extra_nested: &[plakous::model::ToolName],
) -> CliResult<CompiledProviderConfiguration> {
    if extra_nested.is_empty() {
        return Ok(source.clone());
    }
    let mut behavior = source.selected().behavior().clone();
    for name in extra_nested {
        if behavior
            .tools
            .iter()
            .any(|tool| tool.definition().name() == name)
        {
            return Err(CliFailure::new(
                "product Direct catalog contains a duplicate tool",
            ));
        }
        let (Some(definition), Some(registered)) =
            (registry.definition(name), registry.identity(name))
        else {
            return Err(CliFailure::new(
                "product Direct extension is not registered",
            ));
        };
        behavior.tools.push(direct_extension_tool(
            definition.clone(),
            registered.owner(),
        )?);
    }
    behavior
        .tools
        .sort_by(|left, right| left.definition().name().cmp(right.definition().name()));
    let selected =
        SelectedProviderConfiguration::new(source.selected().route().clone(), behavior.clone())
            .map_err(|_| CliFailure::new("extended Direct provider behavior is invalid"))?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        source.effective().descriptor().clone(),
        source.effective().model_implementation().clone(),
        source.effective().authority().clone(),
        behavior,
        source.effective().family_policy().clone(),
    )
    .map_err(|_| CliFailure::new("extended Direct provider configuration could not be compiled"))?;
    CompiledProviderConfiguration::new(selected, effective)
        .map_err(|_| CliFailure::new("extended Direct provider configuration is invalid"))
}

/// The tool plan of one node, bound before anything attaches or compiles;
/// a refusal fails the session's creation, or the spawn, with its one line.
/// MCP servers are started or leased in the node's workspace root.
pub(in crate::agent) fn bind_node_tool_plan(
    tools: &crate::tool_plan::NodeToolSelection,
    route: Option<crate::tool_plan::ToolRoute>,
    route_label: &str,
    workspace_root: &Path,
    state_roots: &ProductStateRoots,
) -> CliResult<crate::tool_plan::ToolPlan> {
    let selection = &tools.selection;
    crate::capabilities::require_tools(selection)?;
    if selection.is_empty() {
        return Ok(crate::tool_plan::ToolPlan::default());
    }
    let route = route.ok_or_else(|| CliFailure::new("native provider is unsupported"))?;
    // Optional tools first: hosted-retrieval wrappers bind on the route's
    // deferred retrieval slot, and a refusal there starts no MCP server.
    // MCP entries then bind on the process pool.
    let plan = crate::tool_plan::bind_tool_plan(selection, route, route_label)
        .map_err(|refusal| CliFailure::new(refusal.to_string()))?
        .with_fetch_url_limits(
            crate::frozen_tables::fetch_url_limits(&tools.tool_sections)
                .map_err(CliFailure::new)?,
        )
        .with_retrieval_options(
            selection,
            route,
            route_label,
            crate::frozen_tables::retrieval_options(&tools.tool_sections)
                .map_err(CliFailure::new)?,
        )
        .map_err(|refusal| CliFailure::new(refusal.to_string()))?;
    let mcp = crate::mcp_binding::bind_mcp_tools(
        selection,
        &tools.tool_sections,
        workspace_root,
        state_roots.named_secrets(),
        tools.mcp_tools.as_deref(),
    )
    .map_err(CliFailure::new)?;
    Ok(plan.with_mcp(selection, mcp))
}

/// One Direct extension as the provider configuration states it. A tool an
/// MCP server executes names that server, so every protocol decodes its calls
/// with that owner before the driver lands them.
pub(crate) fn direct_extension_tool(
    definition: plakous::model::ToolDefinition,
    owner: &plakous::tools::ToolOwnerClass,
) -> CliResult<ConfiguredTool> {
    let tool = ConfiguredTool::new(
        definition,
        ToolExecutionDisposition::Local,
        plakous::model::ToolCallerDisposition::DirectOnly,
    );
    match owner {
        plakous::tools::ToolOwnerClass::PlakLocal => Ok(tool),
        plakous::tools::ToolOwnerClass::Mcp(server) => tool
            .with_mcp_owner(server.canonical().clone())
            .map_err(|_| CliFailure::new("product Direct extension owner is invalid")),
    }
}
