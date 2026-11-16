//! Product-supported direct API tuples, independent of OAuth discovery.
use crate::product_config::ProductCompositionSelection;
use plakous::agent::ReasoningEffort;
use plakous_hosted::binding::PlatformProvider;

/// Every usable model one API route offers offline (the bundled reviewed
/// table under the provider's family rules), in listing order.
#[cfg(test)]
pub(crate) fn models(provider: PlatformProvider) -> Vec<String> {
    plakous_hosted::platform::platform_models(provider)
        .into_iter()
        .map(|row| row.model)
        .collect()
}

/// Whether an exact literal is a usable offline row of one API route.
pub(crate) fn enrolled(provider: PlatformProvider, model: &str) -> bool {
    plakous_hosted::platform::platform_model(provider, model).is_some()
}

/// The effort a template or record spells, when the shared ladder names it.
///
/// Admission against the model's own ladder is separate: `ultra` parses and
/// is then refused by a row that does not publish it.
pub(crate) fn api_effort(name: &str) -> Option<ReasoningEffort> {
    let (effort, explicit) = plakous_hosted::runtime::effort_from_config(Some(name));
    explicit.then_some(effort)
}

/// Admits one API tuple offline, before any session exists: the model must be
/// a usable row of the bundled table, the effort on its published ladder, and
/// the composition one the route admits. Session construction re-admits the
/// model against the provider's live list.
pub(crate) fn validate(
    provider: PlatformProvider,
    model: &str,
    effort: Option<ReasoningEffort>,
    composition: ProductCompositionSelection,
) -> Result<(), String> {
    crate::build_features::require_account(provider.account()).map_err(|error| {
        crate::refusal_text::failure_text(
            &error,
            crate::notice_text::RemedyContext {
                frontend: crate::notice_text::Frontend::Run,
                session_id: None,
            },
        )
    })?;
    plakous_hosted::platform::bundled_catalog(provider, "offline")
        .admit(model, effort)
        .map_err(|error| {
            crate::refusal_text::failure_text(
                &plakous_hosted::CliFailure::from(error),
                crate::notice_text::RemedyContext {
                    frontend: crate::notice_text::Frontend::Run,
                    session_id: None,
                },
            )
        })?;
    plakous_hosted::platform::api_composition(provider, model, composition)
        .map(|_| ())
        .map_err(|error| format!("{}: {error}", provider.account()))
}

/// The composition the root template binds: on a direct API account, what
/// `platform::api_composition` decides for its model (`plakous-default` binds
/// `native-ptc` on a CodeModeOnly OpenAI row). An inadmissible template keeps
/// its selection; config load reports it.
pub(crate) fn root_composition(
    config: &crate::product_config::ProductConfig,
) -> ProductCompositionSelection {
    let selected = config.default_composition();
    match (
        config
            .default_account()
            .and_then(PlatformProvider::for_account),
        config.default_model(),
    ) {
        (Some(provider), Some(model)) => {
            plakous_hosted::platform::api_composition(provider, model, selected).unwrap_or(selected)
        }
        _ => selected,
    }
}

pub(crate) fn entries(
    provider: PlatformProvider,
    composition: ProductCompositionSelection,
) -> Vec<plakous_host_protocol::views::ModelEntry> {
    plakous_hosted::platform::platform_models(provider)
        .iter()
        .map(|row| plakous_host_protocol::views::ModelEntry {
            id: row.model.clone(),
            tag: "API · stream · tools · reasoning".into(),
            effort_options: plakous_hosted::platform::effort_names(&row.efforts)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            extras: vec![],
            reasoning: true,
            unavailable_reason: plakous_hosted::platform::api_composition(
                provider,
                &row.model,
                composition,
            )
            .err()
            .map(str::to_owned),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline product contracts")]
mod tests;
