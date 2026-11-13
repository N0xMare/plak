//! Offline release binding checks without runtime or external-service authority.

use crate::agent::build::coding_tool_registry_for_plan;
use crate::binding::CompositionSelection;
use crate::{CliFailure, CliResult};
use plakous::agent::BindingChange;
use plakous::model::{ReasoningEffortDisposition, SelectedProviderConfiguration, ToolName};
use plakous::sessions::FrozenSessionPresentation;
use plakous::subscriptions::NativeProvider;
use plakous::tools::{PlanStateIdentity, ToolExecutorBindingIdentity, ToolExecutorId};

/// Checks frozen data against this release's executable and native selection
/// metadata. No credential, model, runner, workspace or sidecar is opened.
/// External availability and a later authenticated catalog remain live checks.
///
/// # Errors
/// Returns inspection failures; binding incompatibilities are the typed result.
#[doc(hidden)]
pub fn check_frozen_binding(
    frozen: &FrozenSessionPresentation,
) -> CliResult<Option<BindingChange>> {
    let configuration = frozen.configuration();
    let composition = [
        CompositionSelection::NativePtc,
        CompositionSelection::JsPtc,
        CompositionSelection::PyPtc,
        CompositionSelection::PySdk,
    ]
    .into_iter()
    .find(|candidate| {
        candidate.outer_marker_names().is_some_and(|names| {
            let tools = &configuration.selected().behavior().tools;
            names.len() == tools.len()
                && tools
                    .iter()
                    .all(|tool| names.contains(&tool.definition().name().as_str()))
        })
    })
    .unwrap_or(CompositionSelection::Direct);
    let registry = coding_tool_registry_for_plan(
        stored_plan_identity(frozen)?,
        composition,
        composition == CompositionSelection::NativePtc,
        &crate::tools::ProductToolExtensions::empty(),
    )?;
    if let Some(change) = check_executors(frozen, &registry)? {
        return Ok(Some(change));
    }
    check_native_selection(frozen, composition)
}

fn check_executors(
    frozen: &FrozenSessionPresentation,
    registry: &plakous::tools::FrozenToolRegistry,
) -> CliResult<Option<BindingChange>> {
    for (name, id, recorded) in frozen.executor_requirements().map_err(CliFailure::from)? {
        if id.as_str().starts_with("plakous.mcp.") {
            continue;
        }
        let found = registry
            .identity(&name)
            .filter(|identity| *identity.executor_id() == id)
            .map(plakous::tools::RegisteredToolIdentity::executor_binding_identity)
            .or_else(|| extension_identity(&name, &id));
        let alternatives =
            plakous::tools::web::current_executor_bindings(name.as_str(), id.as_str())
                .map_err(|_| CliFailure::new("optional executable metadata is invalid"))?;
        let found = found
            .or_else(|| {
                alternatives
                    .iter()
                    .copied()
                    .find(|binding| binding.accepts(&id, recorded))
            })
            .or_else(|| alternatives.first().copied());
        match found {
            Some(binding) if binding.accepts(&id, recorded) => {}
            Some(_) => {
                return Ok(Some(BindingChange::ToolBinding {
                    tool: id.to_string(),
                }));
            }
            None if registry.identity(&name).is_some() || release_owned_executor(&id) => {
                return Ok(Some(BindingChange::ToolBinding {
                    tool: id.to_string(),
                }));
            }
            None => {
                return Err(CliFailure::new(format!(
                    "binding inspection cannot establish current executor authority: {id}"
                )));
            }
        }
    }
    Ok(None)
}

fn release_owned_executor(id: &ToolExecutorId) -> bool {
    [
        "plakous.standard.",
        "plakous.process-",
        "plak.topology.",
        "plakous.web.",
        "plak.skills.",
        "plakous.product-javascript-composition.",
        "plakous.python-ptc.",
        "plakous.python-sdk.",
        "plakous.codex-code-mode.",
    ]
    .iter()
    .any(|prefix| id.as_str().starts_with(prefix))
}

fn check_native_selection(
    frozen: &FrozenSessionPresentation,
    composition: CompositionSelection,
) -> CliResult<Option<BindingChange>> {
    let configuration = frozen.configuration();
    let route = configuration.selected().route();
    let model = route.requested_model();
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Claude,
        NativeProvider::Xai,
    ] {
        let Ok(expected_route) =
            plakous::subscriptions::native_provider_route_selection(provider, model.clone())
        else {
            continue;
        };
        if expected_route != *route {
            continue;
        }
        let behavior = configuration.selected().behavior();
        let effort = match behavior.reasoning_effort {
            ReasoningEffortDisposition::Disabled => None,
            ReasoningEffortDisposition::Enabled(effort) => Some(effort),
        };
        let selected = super::selected_coding_provider_configuration_with_processing_tier(
            provider,
            model.clone(),
            effort,
            provider.enrolled_tool_mode(model.as_str()),
            composition,
            behavior.processing_tier.clone(),
        )?;
        let mut current = selected.behavior().clone();
        current.tools.clone_from(&behavior.tools);
        let selected = SelectedProviderConfiguration::new(selected.route().clone(), current)
            .map_err(|_| CliFailure::new("current provider selection could not be inspected"))?;
        if selected.identity() != configuration.selected_identity() {
            return Ok(Some(BindingChange::ProviderConfiguration));
        }
        break;
    }
    Ok(None)
}

fn stored_plan_identity(frozen: &FrozenSessionPresentation) -> CliResult<PlanStateIdentity> {
    let Some(plan) = frozen.executor_id(
        &ToolName::new("update_plan").map_err(|_| CliFailure::new("invalid inspection tool"))?,
    ) else {
        // An unselected plan tool has no stored requirement to compare. This
        // label only builds unused metadata, never a source binding verdict.
        return PlanStateIdentity::new("binding-inspection", 1)
            .map_err(|_| CliFailure::new("invalid inspection plan identity"));
    };
    let (authority, revision) = plan
        .as_str()
        .strip_prefix("plakous.standard.update-plan.v1.")
        .and_then(|name| name.rsplit_once(".r"))
        .ok_or_else(|| {
            CliFailure::new("stored plan-state executor requires an explicit binding cut")
        })?;
    PlanStateIdentity::new(
        authority,
        revision
            .parse()
            .map_err(|_| CliFailure::new("stored plan-state revision is invalid"))?,
    )
    .map_err(|_| CliFailure::new("stored plan-state identity is invalid"))
}

fn extension_identity(name: &ToolName, id: &ToolExecutorId) -> Option<ToolExecutorBindingIdentity> {
    let topology = crate::agent_tree_tools::topology_executor_identity(name.as_str())
        .ok()
        .flatten();
    if let Some((current, identity)) = topology
        && current == *id
    {
        return Some(identity);
    }
    if name.as_str() == crate::skills::SKILL_TOOL_NAME
        && id.as_str() == "plak.skills.skill.executor.v1"
    {
        return crate::skills::skill_executor_identity().ok();
    }
    None
}
