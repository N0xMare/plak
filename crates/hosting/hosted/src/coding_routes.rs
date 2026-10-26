//! The product's route → model-vocabulary catalog.
//!
//! Which route shows a model which tool names is a *product* fact: it depends
//! on which accounts Plak has attached. Plakous holds the rule shape
//! ([`PresentationRoute`]) and the one admission function
//! (`CodingPresentation::admit`); this module holds the rows.
//!
//! Every route reaches that one site — the three native OAuth verticals, the
//! API routes (which wear their OAuth vertical's pack), and every custom
//! integration (OpenRouter), which has no provider-familiar vocabulary of its
//! own and is therefore shown the generic pi pack.

use std::sync::OnceLock;

use plakous::model::CompiledProviderConfiguration;
use plakous::model::ModelId;
use plakous::subscriptions::{NativeProvider, native_provider_route_selection};
use plakous::tools::CodingPresentationExt as _;
use plakous::tools::{
    CodingPresentation, FrozenToolRegistry, PROCESS_PROJECTION_FAMILY, PresentationRoute,
    PresentationRouteTuple, PresentationRoutes, ProjectionCandidateAdmission, ToolProjectionMap,
    ToolRuntimeError, validate_process_presentation_candidate_v2,
};

use crate::binding::PlatformProvider;
use crate::error::{CliFailure, CliResult};
use crate::platform::platform_model;

/// A model id used only to read a vertical's route ids; the route tuple does
/// not depend on it, and no request is built from it.
const ROUTE_PROBE_MODEL: &str = "route-probe";

/// The API route tuples (protocol, route, profile) the catalog rows and
/// [`platform_route_presentation`] both read, so the two cannot drift. The
/// OpenAI profile is per model ([`openai_platform_profile`]).
const OPENAI_PLATFORM: (&str, &str) = ("openai.responses", "openai.platform");
const ANTHROPIC_PLATFORM: (&str, &str, &str) = (
    "anthropic.messages",
    "anthropic.platform",
    "anthropic.platform.adaptive-client-tools.v1",
);
const XAI_PLATFORM: (&str, &str, &str) = (
    "xai.platform.responses.2026-08",
    "xai.platform_api_key",
    "xai.platform.grok-4.6.custom-envelope.v1",
);

/// The product's catalog, built once.
pub(crate) fn product_presentation_routes() -> CliResult<&'static PresentationRoutes> {
    static ROUTES: OnceLock<Option<PresentationRoutes>> = OnceLock::new();
    ROUTES
        .get_or_init(|| build_routes().ok())
        .as_ref()
        .ok_or_else(|| CliFailure::new("product presentation route catalog is invalid"))
}

/// The product's answer to "is this the exact candidate for this route".
///
/// The agent takes this as a port because the answer depends on the pack list
/// the session compiled and the route catalog above — both product facts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProductProjectionAdmission;

impl ProjectionCandidateAdmission for ProductProjectionAdmission {
    fn admit(
        &self,
        job_config: plakous::tools::JobConfig,
        projection: &ToolProjectionMap,
        registry: &FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), ToolRuntimeError> {
        let routes = product_presentation_routes().map_err(|_| {
            ToolRuntimeError::new(
                plakous::tools::ToolRuntimeErrorKind::Registration,
                "product presentation route catalog is invalid",
            )
        })?;
        refuse_retired_topology_names(
            CodingPresentation::admit(configuration, routes).ok(),
            projection,
        )?;
        if validate_process_presentation_candidate_v2(
            job_config,
            projection,
            registry.clone(),
            configuration,
            routes,
        )
        .is_ok()
        {
            return Ok(());
        }
        let presentation = CodingPresentation::admit(configuration, routes)?;
        let extras =
            presentation.projection_extensions(job_config, registry.clone(), projection)?;
        let full = presentation.projection_map_with_extensions(
            job_config,
            registry.clone(),
            projection.target().clone(),
            &extras,
        )?;
        validate_process_presentation_candidate_v2(
            job_config,
            &full,
            registry.clone(),
            configuration,
            routes,
        )?;
        let omitted = full
            .entries()
            .iter()
            .filter(|entry| {
                entry.canonical_operations().values().any(|identity| {
                    registry
                        .definition(identity.name())
                        .is_some_and(|definition| {
                            !crate::agent::occupancy_read_only_allows(
                                definition.name().as_str(),
                                definition.effect(),
                            )
                        })
                })
            })
            .map(|entry| entry.exposed_name().as_str())
            .collect::<Vec<_>>();
        let expected = presentation.projection_map_with_extensions_omitting(
            job_config,
            registry.clone(),
            projection.target().clone(),
            &extras,
            &omitted,
        )?;
        if expected.digest() == projection.digest() {
            Ok(())
        } else {
            Err(ToolRuntimeError::new(
                plakous::tools::ToolRuntimeErrorKind::Registration,
                "projection is not the exact full or occupancy-filtered catalog",
            ))
        }
    }
}

/// W3-B replaced `wait_agent` and `interrupt_agent` on the Grok pack and did
/// not bump the runtime-format epoch. Harness admission refuses a Grok
/// projection that still presents them, so a create that would show the
/// retired names fails closed. Pi is not matched: OpenRouter is
/// `IntegrationClass::Custom` and never reaches [`ProductProjectionAdmission`].
fn refuse_retired_topology_names(
    presentation: Option<CodingPresentation>,
    projection: &ToolProjectionMap,
) -> Result<(), ToolRuntimeError> {
    if matches!(presentation, Some(CodingPresentation::Grok))
        && projection.entries().iter().any(|entry| {
            matches!(
                entry.exposed_name().as_str(),
                "wait_agent" | "interrupt_agent"
            )
        })
    {
        return Err(ToolRuntimeError::new(
            plakous::tools::ToolRuntimeErrorKind::Registration,
            "projection presents wait_agent or interrupt_agent, which this pack's await and interrupt slots replaced",
        ));
    }
    Ok(())
}

/// The tuple of a route that has been selected but not yet compiled.
pub(crate) fn selected_route_presentation(
    provider: NativeProvider,
    requested_model: &str,
) -> CliResult<CodingPresentation> {
    let selection = native_provider_route_selection(
        provider,
        ModelId::new(requested_model)
            .map_err(|_| CliFailure::new("native provider route model is invalid"))?,
    )
    .map_err(|_| CliFailure::new("native provider route is invalid"))?;
    product_presentation_routes()?
        .admit(&PresentationRouteTuple::selected(
            plakous::model::IntegrationClass::Standard,
            selection.protocol().as_str(),
            selection.route().as_str(),
            selection.integration_profile().as_str(),
            requested_model,
        ))
        .map_err(|_| CliFailure::new("native provider is unsupported"))
}

/// The vocabulary an API route admits, from the same catalog a compiled
/// session uses. `requested_model` is the literal the route was selected with.
pub(crate) fn platform_route_presentation(
    provider: PlatformProvider,
    requested_model: &str,
) -> CliResult<CodingPresentation> {
    let (protocol, route, profile) = match provider {
        PlatformProvider::OpenAi => (
            OPENAI_PLATFORM.0,
            OPENAI_PLATFORM.1,
            openai_platform_profile(requested_model)
                .ok_or_else(|| CliFailure::new("OpenAI platform model is not enrolled"))?,
        ),
        PlatformProvider::Anthropic => ANTHROPIC_PLATFORM,
        PlatformProvider::Xai => XAI_PLATFORM,
    };
    product_presentation_routes()?
        .admit(&PresentationRouteTuple::selected(
            plakous::model::IntegrationClass::Standard,
            protocol,
            route,
            profile,
            requested_model,
        ))
        .map_err(|_| CliFailure::new("API route has no enrolled coding vocabulary"))
}

fn build_routes() -> Result<PresentationRoutes, ()> {
    let mut routes = Vec::new();
    for (provider, presentation, family, models) in [
        (
            NativeProvider::Codex,
            CodingPresentation::Codex,
            Some(PROCESS_PROJECTION_FAMILY),
            (|model: &str| NativeProvider::Codex.enrolls(model)) as fn(&str) -> bool,
        ),
        (
            NativeProvider::Xai,
            CodingPresentation::Grok,
            Some(PROCESS_PROJECTION_FAMILY),
            (|model: &str| NativeProvider::Xai.enrolls(model)) as fn(&str) -> bool,
        ),
        (
            NativeProvider::Claude,
            CodingPresentation::Claude,
            None,
            (|model: &str| NativeProvider::Claude.enrolls(model)) as fn(&str) -> bool,
        ),
    ] {
        if provider.is_compiled() {
            routes.push(native_route(provider, presentation, family, models)?);
        }
    }
    routes.extend([
        // The API routes wear their OAuth vertical's pack, without its costume.
        PresentationRoute::standard(
            OPENAI_PLATFORM.0,
            OPENAI_PLATFORM.1,
            "openai.platform.gpt-6.2026-09-22.r1",
            None,
            |model| openai_platform_profile(model) == Some("openai.platform.gpt-6.2026-09-22.r1"),
            platform_presentation(PlatformProvider::OpenAi),
        ),
        PresentationRoute::standard(
            OPENAI_PLATFORM.0,
            OPENAI_PLATFORM.1,
            "openai.platform.gpt-5.6.2026-07-31.r2",
            None,
            |model| openai_platform_profile(model) == Some("openai.platform.gpt-5.6.2026-07-31.r2"),
            platform_presentation(PlatformProvider::OpenAi),
        ),
        PresentationRoute::standard(
            ANTHROPIC_PLATFORM.0,
            ANTHROPIC_PLATFORM.1,
            ANTHROPIC_PLATFORM.2,
            None,
            |model| platform_model(PlatformProvider::Anthropic, model).is_some(),
            platform_presentation(PlatformProvider::Anthropic),
        ),
        PresentationRoute::standard(
            XAI_PLATFORM.0,
            XAI_PLATFORM.1,
            XAI_PLATFORM.2,
            None,
            |model| platform_model(PlatformProvider::Xai, model).is_some(),
            platform_presentation(PlatformProvider::Xai),
        ),
        // OpenRouter and every other custom integration: the product attached
        // the route itself, so it wears the generic pi pack.
        PresentationRoute::custom_integrations(CodingPresentation::Pi),
    ]);
    Ok(PresentationRoutes::new(routes))
}

/// The pack an API route presents: the `openai` API wears the Codex pack, the
/// xAI API the Grok pack and the Anthropic API the Claude pack, as on their
/// OAuth verticals.
pub(crate) const fn platform_presentation(provider: PlatformProvider) -> CodingPresentation {
    match provider {
        PlatformProvider::OpenAi => CodingPresentation::Codex,
        PlatformProvider::Xai => CodingPresentation::Grok,
        PlatformProvider::Anthropic => CodingPresentation::Claude,
    }
}

/// The reviewed OpenAI Platform profile an enrolled literal runs under.
fn openai_platform_profile(model: &str) -> Option<&'static str> {
    crate::platform::openai_profile(model)
}

fn native_route(
    provider: NativeProvider,
    presentation: CodingPresentation,
    family: Option<&'static str>,
    models: fn(&str) -> bool,
) -> Result<PresentationRoute, ()> {
    let selection =
        native_provider_route_selection(provider, ModelId::new(ROUTE_PROBE_MODEL).map_err(|_| ())?)
            .map_err(|_| ())?;
    Ok(PresentationRoute::standard(
        selection.protocol().as_str(),
        selection.route().as_str(),
        selection.integration_profile().as_str(),
        family,
        models,
        presentation,
    ))
}

#[cfg(test)]
mod tests;
