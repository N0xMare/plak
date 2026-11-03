//! Harness profiles and instruction enrollment.

use crate::binding::CompositionSelection;
use crate::{CliFailure, CliResult};
use plakous::agent::{
    ContextSourceCatalog, ContextSourceResolver, ContextSourceSelector, HarnessProfileId,
    HarnessProfileSpec, HarnessProfileSpecBuilder, InstructionCompositionStrategy, ReasoningEffort,
    RegisteredContextSource, ResolvedContextSourceFact,
};
use plakous::model::ModelId;
use plakous::subscriptions::NativeProvider;
use plakous::tools::{CodexMinimalLocalCodeModeV1, CompiledSurface};
use std::path::Path;

/// `pub(crate)` so rendered prompt/wire goldens can compile the same harness
/// spec offline. Production behaviour is unchanged.
#[allow(
    clippy::too_many_arguments,
    reason = "Native harness selection binds provider capability, reasoning effort, Code Mode tools, composition, workspace, and enrolled instructions."
)]
pub(crate) fn selected_harness_profile(
    provider: NativeProvider,
    selected_model: &ModelId,
    effort: impl Into<Option<ReasoningEffort>>,
    opening_durable_session: bool,
    code_mode: bool,
    extra_nested: &[plakous::model::ToolName],
    subagents: bool,
    composition: CompositionSelection,
    surface: &CompiledSurface,
    workspace_root: &Path,
    cwd: &Path,
) -> CliResult<Option<HarnessProfileSpec>> {
    let effort = effort.into();
    if opening_durable_session {
        return Ok(None);
    }
    if code_mode && provider != NativeProvider::Codex {
        return Err(CliFailure::new(
            "Code Mode harness profile is valid only for Codex",
        ));
    }
    let variant = product_prompt_variant();
    let profile = match provider {
        NativeProvider::Xai => plakous::profiles::grok::recommended_for(variant)
            .map_err(|_| CliFailure::new("grok harness profile is invalid"))?,
        NativeProvider::Codex if code_mode => {
            let mut names = CodexMinimalLocalCodeModeV1::nested_names().to_vec();
            names.retain(|name| {
                surface
                    .exposed_names()
                    .iter()
                    .any(|exposed| exposed.as_str() == *name)
            });
            names.extend(extra_nested.iter().map(plakous::model::ToolName::as_str));
            plakous::profiles::codex::code_mode_for(variant, &names)
                .map_err(|_| CliFailure::new("Codex Code Mode harness profile is invalid"))?
        }
        NativeProvider::Codex => plakous::profiles::codex::recommended_for(variant)
            .map_err(|_| CliFailure::new("Codex harness profile is invalid"))?,
        NativeProvider::Claude => plakous::profiles::claude::recommended_for(variant)
            .map_err(|_| CliFailure::new("claude harness profile is invalid"))?,
        _ => {
            return Err(CliFailure::new("native provider is unsupported"));
        }
    };
    let profile = match effort {
        Some(effort) => profile
            .with_preferred_reasoning_effort(effort)
            .map_err(|_| CliFailure::new("harness profile effort override is invalid"))?,
        None => profile,
    };
    product_identity_profile(
        &profile,
        provider,
        selected_model,
        subagents,
        composition,
        surface,
        workspace_root,
        cwd,
    )
    .map(Some)
}

/// The one place `PLAK_PROMPT_VARIANT` is read. The prompts crate takes the
/// variant as an argument everywhere, so the environment is consulted here,
/// at the product layer, and never under construction (spec 01 §2.1).
pub(crate) fn product_prompt_variant() -> plakous::prompts::bases::PromptVariant {
    plakous::prompts::bases::PromptVariant::from_env()
}

fn composition_profile_id(base: &HarnessProfileId, suffix: &str) -> CliResult<HarnessProfileId> {
    HarnessProfileId::new(format!("{}{suffix}", base.as_str()))
        .map_err(|_| CliFailure::new("product composition profile id is invalid"))
}

pub(crate) fn subagent_catalog_profile_id(
    base: &str,
    subagents: bool,
) -> CliResult<HarnessProfileId> {
    let id = if subagents {
        format!("{base}.subagents.v1")
    } else {
        base.to_owned()
    };
    HarnessProfileId::new(id).map_err(|_| CliFailure::new("subagent catalog profile id is invalid"))
}

#[allow(
    clippy::too_many_arguments,
    reason = "The product profile compiler receives the exact model, identity, workspace, instruction sources, and composition declarations."
)]
pub(in crate::agent) fn product_identity_profile(
    profile: &HarnessProfileSpec,
    provider: NativeProvider,
    selected_model: &ModelId,
    subagents: bool,
    composition: CompositionSelection,
    surface: &CompiledSurface,
    workspace_root: &Path,
    cwd: &Path,
) -> CliResult<HarnessProfileSpec> {
    match provider {
        NativeProvider::Codex | NativeProvider::Xai | NativeProvider::Claude => {}
        _ => return Err(CliFailure::new("native provider is unsupported")),
    }
    product_target_identity_profile(
        profile,
        selected_model.as_str(),
        subagents,
        composition,
        surface,
        workspace_root,
        cwd,
    )
}

/// The product's first-turn instruction profile for one target: the family
/// base (or the PTC composition's replacement of it) with the identity tail
/// and the paragraphs the compiled surface earned, assembled once by
/// `prompts::plan::target_instructions`. A PTC composition also suffixes the
/// profile id (`.js-ptc.v1` / `.py-ptc.v1`) so the checkpoint plan names the
/// composition it was compiled for.
pub(crate) fn product_target_identity_profile(
    profile: &HarnessProfileSpec,
    selected_model: &str,
    subagents: bool,
    composition: CompositionSelection,
    surface: &CompiledSurface,
    workspace_root: &Path,
    cwd: &Path,
) -> CliResult<HarnessProfileSpec> {
    let profile_id = subagent_catalog_profile_id(profile.id().as_str(), subagents)?;
    let profile_id = match composition {
        CompositionSelection::JsPtc => composition_profile_id(&profile_id, ".js-ptc.v1")?,
        CompositionSelection::PyPtc => composition_profile_id(&profile_id, ".py-ptc.v1")?,
        CompositionSelection::PySdk => composition_profile_id(&profile_id, ".py-sdk.v1")?,
        CompositionSelection::Direct
        | CompositionSelection::FollowCatalog
        | CompositionSelection::NativePtc => profile_id,
    };
    // The environment block states the effort the profile asked for, the way
    // Codex states it in its turn context. The label is the prompts crate's
    // word, not this crate's.
    let selected_effort = profile
        .preferred_reasoning_effort()
        .map(plakous::prompts::identity::product_effort_label);
    let base_instructions = plakous::prompts::plan::target_instructions(
        profile.base_instructions(),
        crate::prompt_inputs::prompt_composition(composition),
        surface,
        workspace_root,
        cwd,
        selected_model,
        selected_effort.as_deref(),
    );
    let mut builder = HarnessProfileSpecBuilder::new()
        .id(profile_id)
        .semantic_version(profile.semantic_version())
        .provenance(profile.provenance())
        .base_instructions(base_instructions)
        .instruction_strategy(profile.instruction_strategy())
        .instruction_sources(profile.instruction_sources().to_vec())
        .tool_exposure_strategy(profile.tool_exposure_strategy())
        .tool_overrides(profile.tool_overrides().to_vec())
        .reasoning_strategy(profile.reasoning_strategy())
        .preferred_reasoning_effort(profile.preferred_reasoning_effort())
        .continuation_strategy(profile.continuation_strategy())
        .cache_strategy(profile.cache_strategy());
    if let Some(display_name) = profile.display_name() {
        builder = builder.display_name(display_name);
    }
    builder
        .build()
        .map_err(|error| CliFailure::new(format!("product identity profile is invalid: {error}")))
}

/// Enrolls the discovered `AGENTS.md` files, then the node's skills listing
/// when it has one, as the profile's instruction sources (spec 02 §6.1).
pub(crate) fn enroll_product_instructions(
    profile: &HarnessProfileSpec,
    product_home: Option<&Path>,
    workspace_root: &Path,
    cwd: &Path,
    skills_listing: Option<&RegisteredContextSource>,
) -> CliResult<(HarnessProfileSpec, Vec<ResolvedContextSourceFact>)> {
    let reader = crate::runner_reader::instruction_reader(product_home);
    let discovery = plakous::prompts::instructions::discover_instruction_sources(
        reader.as_ref(),
        product_home,
        workspace_root,
        cwd,
    )
    .map_err(|error| CliFailure::new(format!("product instructions are invalid: {error}")))?;
    let mut sources = Vec::with_capacity(discovery.sources().len() + 1);
    for source in discovery.sources() {
        sources.push(source.registered_source().map_err(|error| {
            CliFailure::new(format!(
                "instruction source {} could not be registered: {error}",
                source.source_id()
            ))
        })?);
    }
    sources.extend(skills_listing.cloned());
    if sources.is_empty() {
        return Ok((profile.clone(), Vec::new()));
    }

    let mut catalog = ContextSourceCatalog::new();
    let mut selectors = Vec::with_capacity(sources.len());
    for registered in sources {
        selectors.push(
            ContextSourceSelector::new(registered.source_id()).map_err(|error| {
                CliFailure::new(format!(
                    "instruction source {} has an invalid selector: {error}",
                    registered.source_id()
                ))
            })?,
        );
        catalog.insert(registered).map_err(|error| {
            CliFailure::new(format!("instruction source catalog is invalid: {error}"))
        })?;
    }
    let resolved = ContextSourceResolver::new(catalog)
        .resolve(&selectors)
        .map_err(|error| {
            CliFailure::new(format!("instruction sources could not resolve: {error}"))
        })?;

    let mut builder = HarnessProfileSpecBuilder::new()
        .id(profile.id().clone())
        .semantic_version(profile.semantic_version())
        .provenance(profile.provenance())
        .base_instructions(profile.base_instructions())
        .instruction_strategy(InstructionCompositionStrategy::BaseThenSources)
        .instruction_sources(selectors)
        .tool_exposure_strategy(profile.tool_exposure_strategy())
        .tool_overrides(profile.tool_overrides().to_vec())
        .reasoning_strategy(profile.reasoning_strategy())
        .preferred_reasoning_effort(profile.preferred_reasoning_effort())
        .continuation_strategy(profile.continuation_strategy())
        .cache_strategy(profile.cache_strategy());
    if let Some(display_name) = profile.display_name() {
        builder = builder.display_name(display_name);
    }
    let enrolled = builder
        .build()
        .map_err(|error| CliFailure::new(format!("instruction profile is invalid: {error}")))?;
    Ok((enrolled, resolved))
}
