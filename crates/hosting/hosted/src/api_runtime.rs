//! Shared local-tool runtime construction for separately billed API routes.

use std::path::Path;
use std::sync::Arc;

use plakous::agent::{
    AgentBuilder, AgentConfig, AgentRequestPolicy, CachePreferenceStrategy, CapabilityRequirement,
    ContinuationPreferenceStrategy, GenerationLimits, HarnessProfileProvenance,
    HarnessProfileSpecBuilder, InstructionCompositionStrategy, ProfileSemanticVersion,
    ReasoningEffort, ReasoningPolicy, ReasoningPreferenceStrategy, ToolExposureStrategy,
    compile_explicit_runtime_profile,
};
use plakous::model::{
    ConfiguredLanguageModel, ConfiguredTool, ContextAdmissionRequirement, ToolCallerDisposition,
    ToolExecutionDisposition,
};
use plakous::sessions::SessionRuntimeBinding;
use plakous::tools::CodingPresentationExt as _;
use plakous::tools::{
    CodexMinimalLocalCodeModeV1, CodingPresentation, CompiledSurface,
    ProductJavaScriptCompositionV1, PythonPtcPresentationV1, PythonPtcV1,
};

use crate::agent::{
    NATIVE_RUN_MODEL_CLEANUP_TIMEOUT, NATIVE_RUN_MODEL_OPERATION_TIMEOUT, SessionToolFence,
    enroll_product_instructions, product_javascript_candidate_attachment,
    product_python_attachment, projection_target, session_tool_policy, stamped_workspace_policy,
    subagent_catalog_profile_id,
};
use crate::binding::{CompositionSelection, ResolvedProtocol};
use crate::error::{CliFailure, CliResult};
use crate::tools::product_surface_with_fence;
use plakous::prompts::plan::target_instructions;

#[derive(Clone, Copy)]
pub(crate) struct ApiRuntimeSpec<'a> {
    pub job_config: plakous::tools::JobConfig,
    pub account: &'a str,
    pub sidecars: &'a dyn plakous::tools::SidecarLocator,
    pub product_home: Option<&'a Path>,
    pub literal_model: &'a str,
    pub effort: Option<ReasoningEffort>,
    pub composition: CompositionSelection,
    pub wire: ResolvedProtocol,
    pub workspace_root: &'a Path,
    pub cwd: &'a Path,
    pub plan_state_authority: &'a str,
    pub opening_durable_session: bool,
    pub tool_fence: SessionToolFence,
    pub scheduled_compaction: bool,
    /// Product `turn.max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    pub topology_extensions: &'a crate::tools::ProductToolExtensions,
    pub telemetry: Option<&'a Arc<plakous::agent::TelemetryEmitter>>,
    pub spawned_child: bool,
}

pub(crate) fn runtime_binding(
    spec: &ApiRuntimeSpec<'_>,
    configured: ConfiguredLanguageModel,
    registry: plakous::tools::FrozenToolRegistry,
) -> CliResult<SessionRuntimeBinding> {
    let _binding_span = tracing::info_span!(
        "api_runtime_binding",
        provider = spec.account,
        model = spec.literal_model,
        composition = spec.composition.as_str(),
        session_id = spec.plan_state_authority,
    )
    .entered();
    let physical_root = std::fs::canonicalize(spec.workspace_root)
        .map_err(|_| CliFailure::new("recorded workspace root could not be resolved"))?;
    let physical_cwd = std::fs::canonicalize(spec.cwd)
        .map_err(|_| CliFailure::new("workspace current directory could not be resolved"))?;
    let relative_cwd = physical_cwd.strip_prefix(&physical_root).map_err(|_| {
        CliFailure::new("workspace current directory is outside the recorded workspace root")
    })?;
    let workspace = Arc::new(stamped_workspace_policy(&physical_root, relative_cwd)?);
    let configuration = configured.configuration().clone();
    let configured_for_composition = configured.clone();
    // Maintained Platform and gateway Custom routes share the same product
    // projection admission boundary and exact session effect policy.
    let mut builder = AgentBuilder::new(configured, registry.clone(), Arc::clone(&workspace))
        .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
        .post_compaction_note(Arc::new(plakous::tools::plan_restoration_note))
        .tool_policy(session_tool_policy(spec.tool_fence))
        .host_process_options(crate::process_environment::options(&physical_root)?)
        .job_config(spec.job_config);
    if let Some(telemetry) = spec.telemetry {
        builder = builder.telemetry(Arc::clone(telemetry));
    }
    let target = projection_target(&configuration, "API target")?;
    // The product catalog admits the exact configured route to its vocabulary.
    let presentation = CodingPresentation::admit(
        &configuration,
        crate::coding_routes::product_presentation_routes()?,
    )
    .map_err(|_| CliFailure::new("API route has no enrolled coding vocabulary"))?;
    // The callable surface for the paragraphs and the PTC base: the core menu
    // plus every topology extension, whether the request carries them Direct
    // or only inside cells.
    let surface = product_surface_with_fence(
        spec.job_config,
        presentation,
        registry.clone(),
        target.clone(),
        spec.topology_extensions,
        spec.tool_fence,
        spec.composition.runtime(),
    )?;
    match spec.composition {
        CompositionSelection::Direct | CompositionSelection::FollowCatalog => {
            if !spec.opening_durable_session {
                let extra: Vec<_> = spec.topology_extensions.names().cloned().collect();
                let full = presentation
                    .projection_map_with_extensions(
                        spec.job_config,
                        registry.clone(),
                        target.clone(),
                        &extra,
                    )
                    .map_err(|error| {
                        CliFailure::new(format!("structured tools are invalid: {error}"))
                    })?;
                let mut omitted = full
                    .entries()
                    .iter()
                    .filter(|entry| !surface.exposed_names().contains(entry.exposed_name()))
                    .map(|entry| entry.exposed_name().as_str())
                    .collect::<Vec<_>>();
                if spec.wire == ResolvedProtocol::Chat {
                    omitted.push("inspect_image");
                }
                let projection = if omitted.is_empty() {
                    Ok(full.clone())
                } else {
                    presentation.projection_map_with_extensions_omitting(
                        spec.job_config,
                        registry.clone(),
                        target,
                        &extra,
                        &omitted,
                    )
                }
                .map_err(|error| {
                    CliFailure::new(format!("structured Chat tools are invalid: {error}"))
                })?;
                builder = builder.tool_projection_v2(projection);
            }
        }
        CompositionSelection::JsPtc => {
            let descriptor = ProductJavaScriptCompositionV1::compile_for_kind(
                spec.job_config,
                registry.clone(),
                target,
                presentation,
                surface.exposed_names().iter().cloned(),
            )
            .map_err(|error| {
                CliFailure::new(format!("JavaScript composition is invalid: {error}"))
            })?;
            let host_path = spec.sidecars.javascript_host().map_err(|error| {
                CliFailure::new(format!(
                    "JavaScript host is unavailable: {error}; set PLAK_JS_HOST or place plakous-js-host beside plak"
                ))
            })?;
            let attachment = product_javascript_candidate_attachment(
                &descriptor,
                &host_path,
                &configuration,
                presentation,
            )
            .ok_or_else(|| CliFailure::new("JavaScript artifact is unavailable"))?;
            builder = plakous::agent::attach_local_composition(builder, Some(attachment)).map_err(
                |error| CliFailure::new(format!("JavaScript artifact is unavailable: {error}")),
            )?;
        }
        CompositionSelection::PyPtc => {
            let descriptor = PythonPtcV1::compile_for_kind(
                spec.job_config,
                registry.clone(),
                target,
                PythonPtcPresentationV1::StructuredFunctions,
                presentation,
                surface.exposed_names().iter().cloned(),
            )
            .map_err(|error| CliFailure::new(format!("Python composition is invalid: {error}")))?;
            let host = spec.sidecars.python_host().map_err(|error| {
                CliFailure::new(format!(
                    "Python host is unavailable: {error}; set PLAK_MONTY_WORKER or place monty beside plak"
                ))
            })?;
            let attachment =
                product_python_attachment(&descriptor, &host, &configuration, presentation)?;
            builder = plakous::agent::attach_local_composition(builder, Some(attachment)).map_err(
                |error| CliFailure::new(format!("Python attachment is invalid: {error}")),
            )?;
        }
        CompositionSelection::NativePtc => {
            let descriptor = CodexMinimalLocalCodeModeV1::compile_for_catalog(
                spec.job_config,
                registry.clone(),
                target,
                &code_mode_nested_names(&surface),
            )
            .map_err(|error| {
                CliFailure::new(format!("Code Mode descriptor is invalid: {error}"))
            })?;
            let host_path = spec.sidecars.javascript_host().map_err(|error| {
                CliFailure::new(format!(
                    "Codex Code Mode sidecar is unavailable: {error}; set PLAK_JS_HOST or place plakous-js-host beside plak"
                ))
            })?;
            builder = plakous::agent::codex_minimal_local_code_mode_sidecar(
                builder,
                &descriptor,
                host_path,
            )
            .map_err(|error| {
                CliFailure::new(format!("Codex Code Mode could not attach: {error}"))
            })?;
        }
    }
    let limits = behavior_generation_limits(configuration.effective().behavior())?;
    let reasoning = match spec.effort {
        Some(effort) => ReasoningPolicy::Enabled {
            effort: Some(effort),
            requirement: CapabilityRequirement::Required,
        },
        None => ReasoningPolicy::Disabled,
    };
    let request = AgentRequestPolicy::new(limits, reasoning)
        .with_context_admission_requirement(ContextAdmissionRequirement::AnyBounded);
    let config = AgentConfig::default()
        .with_max_model_rounds(spec.max_model_rounds)
        .with_model_cleanup_timeout(NATIVE_RUN_MODEL_CLEANUP_TIMEOUT)
        .with_model_operation_timeout(NATIVE_RUN_MODEL_OPERATION_TIMEOUT)
        .with_event_capacity(4_096)
        .with_event_byte_capacity(16 * 1024 * 1024)
        .with_scheduled_compaction_enabled(spec.scheduled_compaction);
    if !spec.opening_durable_session {
        let profile = api_product_profile(
            spec.account,
            spec.composition,
            spec.effort,
            spec.wire,
            spec.literal_model,
            !spec.topology_extensions.is_empty(),
            workspace.root(),
            workspace.cwd(),
            &surface,
        )?;
        let (profile, context_sources) = enroll_product_instructions(
            &profile,
            spec.product_home,
            workspace.root(),
            workspace.cwd(),
        )?;
        let model = configured_for_composition.model();
        let plan = compile_explicit_runtime_profile(
            &profile,
            model.as_ref(),
            &configuration,
            &registry,
            workspace.as_ref(),
            context_sources,
        )
        .map_err(|error| {
            CliFailure::new(format!("API harness plan could not be compiled: {error}"))
        })?;
        builder = builder.harness_plan(plan);
    }
    if spec.spawned_child {
        builder = plakous::sessions::isolated_tool_scheduler(builder).map_err(|error| {
            CliFailure::new(format!(
                "spawn child tool scheduler is invalid ({:?}): {error}",
                error.kind()
            ))
        })?;
    }
    Ok(SessionRuntimeBinding::new(
        builder.config(config).request_policy(request),
    ))
}

/// Session-open API harness profile: the Direct base or the PTC harness
/// base, the identity tail, and the paragraphs the surface earned.
///
/// Base selection has one site — `target_instructions` — so API picks
/// no base of its own; it names the composition in the profile id and hands
/// the Direct base down as the family base.
///
/// `pub(crate)` so the rendered-prompt goldens can assemble the same spec offline
/// without credentials. Production behaviour is unchanged.
pub(crate) fn api_product_profile(
    account: &str,
    composition: CompositionSelection,
    effort: Option<ReasoningEffort>,
    wire: ResolvedProtocol,
    literal_model: &str,
    subagents: bool,
    workspace_root: &Path,
    cwd: &Path,
    surface: &CompiledSurface,
) -> CliResult<plakous::agent::HarnessProfileSpec> {
    let wire_name = match wire {
        ResolvedProtocol::Chat => "chat",
        ResolvedProtocol::Responses => "responses",
        ResolvedProtocol::Messages => "messages",
    };
    let composition_name = match composition {
        CompositionSelection::Direct | CompositionSelection::FollowCatalog => "direct",
        CompositionSelection::JsPtc => "js-ptc",
        CompositionSelection::PyPtc => "py-ptc",
        CompositionSelection::NativePtc => "native-ptc",
    };
    let id = subagent_catalog_profile_id(
        &format!("plak.{account}.{wire_name}.{composition_name}.v1"),
        subagents,
    )?;
    let selected_effort = effort.map(plakous::prompts::identity::product_effort_label);
    let instructions = target_instructions(
        &api_base_instructions(account, composition, surface)?,
        crate::prompt_inputs::prompt_composition(composition),
        surface,
        workspace_root,
        cwd,
        literal_model,
        selected_effort.as_deref(),
    );
    HarnessProfileSpecBuilder::new()
        .id(id)
        .semantic_version(ProfileSemanticVersion::new(0, 1, 0))
        .provenance(HarnessProfileProvenance::Operator)
        .base_instructions(instructions)
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .reasoning_strategy(if effort.is_some() {
            ReasoningPreferenceStrategy::PreferEffort
        } else {
            ReasoningPreferenceStrategy::Disabled
        })
        .preferred_reasoning_effort(effort)
        .continuation_strategy(ContinuationPreferenceStrategy::CanonicalReplayOnly)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()
        .map_err(|error| CliFailure::new(format!("API product profile is invalid: {error}")))
}

/// The family base an API session starts from: the base of the OAuth pack the
/// route wears (Codex, its Code Mode variant, Grok or Claude), or the pi base
/// over the surface's pi tools.
fn api_base_instructions(
    account: &str,
    composition: CompositionSelection,
    surface: &CompiledSurface,
) -> CliResult<String> {
    let variant = crate::agent::product_prompt_variant();
    let presentation = crate::binding::PlatformProvider::for_account(account)
        .map(crate::coding_routes::platform_presentation);
    let pack = match (presentation, composition) {
        (Some(CodingPresentation::Codex), CompositionSelection::NativePtc) => {
            let names = code_mode_nested_names(surface);
            let names = names
                .iter()
                .map(plakous::model::ToolName::as_str)
                .collect::<Vec<_>>();
            plakous::profiles::codex::code_mode_for(variant, &names)
        }
        (_, CompositionSelection::NativePtc) => {
            return Err(CliFailure::new("API native-ptc composition is unavailable"));
        }
        (Some(CodingPresentation::Codex), _) => plakous::profiles::codex::recommended_for(variant),
        (Some(CodingPresentation::Grok), _) => plakous::profiles::grok::recommended_for(variant),
        (Some(CodingPresentation::Claude), _) => {
            plakous::profiles::claude::recommended_for(variant)
        }
        _ => {
            return Ok(plakous::prompts::bases::pi_base_instructions(
                surface.exposed_names(),
            ));
        }
    };
    let pack = pack.map_err(|_| CliFailure::new("API pack harness profile is invalid"))?;
    Ok(pack.base_instructions().to_owned())
}

/// The nested Code Mode catalog in Codex OAuth order: the minimal core
/// (`exec_command`, `write_stdin`, `apply_patch`, `update_plan`) for the names
/// this surface presents, then every other exposed name except `inspect_image`.
///
/// `compile_for_catalog` already emits that order. The prompt `{names}` list
/// has to use it too, or the model is told a different sequence than the cell.
pub(crate) fn code_mode_nested_names(surface: &CompiledSurface) -> Vec<plakous::model::ToolName> {
    let exposed = surface.exposed_names();
    let mut names = Vec::new();
    for name in CodexMinimalLocalCodeModeV1::nested_names() {
        if let Some(found) = exposed.iter().find(|exposed| exposed.as_str() == *name) {
            names.push((*found).clone());
        }
    }
    for name in exposed {
        if name.as_str() == "inspect_image" || names.iter().any(|have| have == name) {
            continue;
        }
        names.push(name.clone());
    }
    names
}

/// The generation limits one effective provider behavior implies.
///
/// The agent revalidates the request policy against the effective provider
/// behavior, so the bound has to come from the same place the behavior got it.
/// A route that derives its own bound from discovery, as OpenRouter does, would
/// otherwise disagree with a product constant read here and the session would
/// refuse to initialize before any dispatch. One reader, one value.
pub(crate) fn behavior_generation_limits(
    behavior: &plakous::model::ProviderBehaviorConfiguration,
) -> CliResult<GenerationLimits> {
    let output_bound = behavior.output_bound.bound();
    GenerationLimits::new(output_bound, Vec::new())
        .map_err(|_| CliFailure::new("API generation limits are invalid"))
}

/// The provider-visible tools of an API session: the composition's outer
/// markers, or the route presentation's core menu plus Direct extras.
pub(crate) fn configured_tools(
    presentation: CodingPresentation,
    composition: CompositionSelection,
    wire: ResolvedProtocol,
    registry: &plakous::tools::FrozenToolRegistry,
    extra_names: &[&str],
) -> CliResult<Vec<ConfiguredTool>> {
    let canonical = composition
        .outer_marker_names()
        .unwrap_or_else(|| presentation.configured_canonical_names());
    let mut selected_names = canonical.to_vec();
    if matches!(
        composition,
        CompositionSelection::Direct | CompositionSelection::FollowCatalog
    ) {
        for name in extra_names {
            if !selected_names.contains(name) {
                selected_names.push(*name);
            }
        }
        if matches!(wire, ResolvedProtocol::Chat) {
            selected_names.retain(|name| *name != "inspect_image");
        }
    }
    let tools = registry
        .definitions()
        .iter()
        .filter(|definition| selected_names.contains(&definition.name().as_str()))
        .cloned()
        .map(|definition| {
            ConfiguredTool::new(
                definition,
                ToolExecutionDisposition::Local,
                ToolCallerDisposition::DirectOnly,
            )
        })
        .collect::<Vec<_>>();
    if tools.len() != selected_names.len() {
        return Err(CliFailure::new(
            "API coding capability is not registered exactly",
        ));
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `native-ptc` is refused before any non-Codex pack's base is chosen:
    /// the Anthropic and xAI APIs and OpenRouter never fall through to their
    /// Direct base under it.
    #[test]
    fn native_ptc_is_refused_before_a_non_codex_base() {
        for (account, presentation) in [
            ("anthropic", CodingPresentation::Claude),
            ("xai-api", CodingPresentation::Grok),
            ("openrouter", CodingPresentation::Pi),
        ] {
            let surface = crate::tools::test_support::surface(presentation, &[]);
            assert!(
                api_base_instructions(account, CompositionSelection::NativePtc, &surface).is_err(),
                "{account}"
            );
            assert!(
                api_base_instructions(account, CompositionSelection::Direct, &surface).is_ok(),
                "{account}"
            );
        }
    }
}
