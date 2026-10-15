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
    Ok(PresentationRoutes::new([
        native_route(
            NativeProvider::Codex,
            CodingPresentation::Codex,
            Some(PROCESS_PROJECTION_FAMILY),
            |model| NativeProvider::Codex.enrolls(model),
        )?,
        native_route(
            NativeProvider::Xai,
            CodingPresentation::Grok,
            Some(PROCESS_PROJECTION_FAMILY),
            |model| NativeProvider::Xai.enrolls(model),
        )?,
        // Claude OAuth admits every family its catalog resolves; the model
        // list is the enrolled candidate set.
        native_route(
            NativeProvider::Claude,
            CodingPresentation::Claude,
            None,
            |model| NativeProvider::Claude.enrolls(model),
        )?,
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
    ]))
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
    plakous::openai::OpenAiPlatformModelFacts::from_model_id(model)
        .map(plakous::openai::OpenAiPlatformModelFacts::profile_id)
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
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use super::{product_presentation_routes, selected_route_presentation};
    use plakous::model::ModelId;
    use plakous::subscriptions::NativeProvider;
    use plakous::tools::CodingPresentation;

    const CODEX_PROCESS_PROJECTION_MODEL: &str = "gpt-5.6-luna";
    const GROK_PROCESS_PROJECTION_MODEL: &str = "grok-4.5";

    #[test]
    fn every_enrolled_vertical_reaches_its_vocabulary_through_one_catalog() {
        for (provider, model, expected) in [
            (
                NativeProvider::Codex,
                CODEX_PROCESS_PROJECTION_MODEL,
                CodingPresentation::Codex,
            ),
            (
                NativeProvider::Xai,
                GROK_PROCESS_PROJECTION_MODEL,
                CodingPresentation::Grok,
            ),
            (
                NativeProvider::Claude,
                "claude-sonnet-5",
                CodingPresentation::Claude,
            ),
        ] {
            assert_eq!(
                selected_route_presentation(provider, model).expect("presentation"),
                expected,
                "{provider:?}"
            );
        }
    }

    /// A model admitted in one place and missing from another fails here.
    /// Every model a provider table enrolls, on every route, must reach a
    /// presentation; a native model must also build a coding selection and an
    /// API model must carry usable limits and pass attach admission. This is
    /// the check that would have caught the Grok 4.7 Fast child failure
    /// (a model enrolled by its provider but missing from a second list).
    #[test]
    fn every_enrolled_model_on_every_route_is_usable_end_to_end() {
        use crate::binding::{CompositionSelection, PlatformProvider};
        use crate::platform::platform_models;
        use plakous::agent::ReasoningEffort;
        use plakous::model::IntegrationClass;
        use plakous::tools::PresentationRouteTuple;

        for provider in [
            NativeProvider::Codex,
            NativeProvider::Xai,
            NativeProvider::Claude,
        ] {
            let models = provider.enrolled_models();
            assert!(!models.is_empty(), "{provider:?} enrolls nothing");
            for model in models {
                selected_route_presentation(provider, model)
                    .unwrap_or_else(|error| panic!("{provider:?} {model}: {error:?}"));
                crate::agent::selected_coding_provider_configuration(
                    provider,
                    ModelId::new(model).expect("model"),
                    ReasoningEffort::Low,
                    provider.enrolled_tool_mode(model),
                    CompositionSelection::FollowCatalog,
                )
                .unwrap_or_else(|error| panic!("{provider:?} {model}: {error:?}"));
            }
        }

        let routes = product_presentation_routes().expect("catalog");
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            let rows = platform_models(provider);
            assert!(!rows.is_empty(), "{provider:?} enrolls nothing");
            for row in rows {
                assert!(
                    row.window(crate::platform::ApiWindow::Working) > 65_536,
                    "{}",
                    row.model
                );
                assert!(row.output_bound() > 4_096, "{}", row.model);
                assert!(row.output_bound() <= row.max_output_tokens, "{}", row.model);
                let (protocol, route, profile) = match provider {
                    PlatformProvider::OpenAi => (
                        "openai.responses",
                        "openai.platform",
                        super::openai_platform_profile(&row.model).expect("profile"),
                    ),
                    PlatformProvider::Anthropic => (
                        "anthropic.messages",
                        "anthropic.platform",
                        "anthropic.platform.adaptive-client-tools.v1",
                    ),
                    PlatformProvider::Xai => (
                        "xai.platform.responses.2026-08",
                        "xai.platform_api_key",
                        "xai.platform.grok-4.6.custom-envelope.v1",
                    ),
                };
                let tuple = PresentationRouteTuple::selected(
                    IntegrationClass::Standard,
                    protocol,
                    route,
                    profile,
                    &row.model,
                );
                let expected = match provider {
                    PlatformProvider::OpenAi => CodingPresentation::Codex,
                    PlatformProvider::Xai => CodingPresentation::Grok,
                    PlatformProvider::Anthropic => CodingPresentation::Claude,
                };
                assert_eq!(
                    routes.admit(&tuple).expect("API presentation"),
                    expected,
                    "{}",
                    row.model
                );
                assert!(
                    crate::runtime::account_model_compatible(provider.account(), &row.model),
                    "{}",
                    row.model
                );
            }
        }
    }

    #[test]
    fn a_route_whose_model_is_not_enrolled_is_refused() {
        assert!(selected_route_presentation(NativeProvider::Codex, "gpt-3").is_err());
        // A vertical's route with another vertical's model is not a rule.
        assert!(
            selected_route_presentation(NativeProvider::Claude, GROK_PROCESS_PROJECTION_MODEL)
                .is_err()
        );
        assert!(ModelId::new("gpt-3").is_ok());
    }

    #[test]
    fn a_custom_integration_is_admitted_to_the_pi_vocabulary() {
        // OpenRouter attaches its own custom integration; there is no
        // provider-familiar vocabulary for a route the product created.
        let routes = product_presentation_routes().expect("catalog");
        let tuple = plakous::tools::PresentationRouteTuple::selected(
            plakous::model::IntegrationClass::Custom,
            "openrouter.chat-completions.v1",
            "openrouter.platform",
            "openrouter.chat.dynamic-v1",
            "anything/at-all",
        );
        assert_eq!(routes.admit(&tuple).expect("pi"), CodingPresentation::Pi);
    }

    fn topology_extensions(
        vocabulary: crate::agent_tree_tools::TopologyVocabulary,
    ) -> crate::tools::ProductToolExtensions {
        let directory = tempfile::tempdir().expect("tempdir");
        let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            "tree-coding-routes",
            "root",
            "owner",
            directory.path().to_path_buf(),
            "selected-tuple",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .expect("tree");
        crate::topology_extensions::topology_tool_extensions_in(
            std::sync::Arc::new(std::sync::Mutex::new(tree)),
            "root",
            vocabulary,
        )
        .expect("extensions")
    }

    fn fixture_target() -> plakous::tools::ProviderModelProjectionIdentityV2 {
        let effective = serde_json::from_value(
            serde_json::to_value(
                plakous::model::Digest::sha256(
                    plakous::model::DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"coding-routes-hub-fixture"],
                )
                .expect("digest"),
            )
            .expect("digest json"),
        )
        .expect("effective identity");
        plakous::tools::ProviderModelProjectionIdentityV2::new(
            plakous::model::IntegrationClass::Standard,
            "fixture.responses",
            "fixture.route",
            "fixture.profile",
            "responses-family",
            "fixture-model",
            "fixture-model",
            effective,
            plakous::tools::PROCESS_TOOL_PROJECTION_REVISION,
        )
        .expect("target")
    }

    /// The Direct hub projection a pack compiles over `extensions`.
    fn hub_projection(
        presentation: CodingPresentation,
        extensions: &crate::tools::ProductToolExtensions,
    ) -> (
        plakous::tools::FrozenToolRegistry,
        plakous::tools::ToolProjectionMap,
    ) {
        use plakous::tools::CodingPresentationExt as _;
        let registry = crate::agent::coding_tool_registry_with_extensions(
            "fixture-coding-routes",
            crate::binding::CompositionSelection::Direct,
            false,
            extensions,
        )
        .expect("registry");
        let names = extensions.names().cloned().collect::<Vec<_>>();
        let projection = presentation
            .projection_map_with_extensions(
                plakous::tools::JobConfig::default(),
                registry.clone(),
                fixture_target(),
                &names,
            )
            .expect("hub projection");
        (registry, projection)
    }

    /// Every enrolled API row reaches its vertical's pack through the shared
    /// route tuples, and an unenrolled literal is an error, never a silent
    /// Plak vocabulary.
    #[test]
    fn every_enrolled_api_row_admits_its_pack_and_a_miss_is_an_error() {
        use crate::binding::PlatformProvider;
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            for row in crate::platform::platform_models(provider) {
                assert_eq!(
                    super::platform_route_presentation(provider, &row.model).expect("enrolled"),
                    super::platform_presentation(provider),
                    "{provider:?}/{}",
                    row.model
                );
            }
            assert!(
                super::platform_route_presentation(provider, "unenrolled-model").is_err(),
                "{provider:?}"
            );
        }
    }

    /// A Grok session recorded while `wait_agent` and `interrupt_agent` were
    /// presented is refused; the claimed vocabulary and the Codex pack's Plak
    /// names pass this guard. Pi is not guarded here.
    #[test]
    fn grok_projections_presenting_the_retired_names_are_refused() {
        use crate::agent_tree_tools::TopologyVocabulary;
        let plak = topology_extensions(TopologyVocabulary::Plak);
        let (_, retired) = hub_projection(CodingPresentation::Grok, &plak);
        let error = super::refuse_retired_topology_names(Some(CodingPresentation::Grok), &retired)
            .expect_err("a retired hub projection passed");
        assert!(
            error.to_string().contains("wait_agent or interrupt_agent"),
            "{error}"
        );
        let (_, claimed) = hub_projection(
            CodingPresentation::Grok,
            &topology_extensions(TopologyVocabulary::TaskTools),
        );
        super::refuse_retired_topology_names(Some(CodingPresentation::Grok), &claimed)
            .expect("the claimed vocabulary passes");
        let (_, codex) = hub_projection(CodingPresentation::Codex, &plak);
        super::refuse_retired_topology_names(Some(CodingPresentation::Codex), &codex)
            .expect("the Plak vocabulary is current on the Codex pack");
    }

    /// A Grok or pi hub's claimed slots survive the durable registry round
    /// trip and are recompiled as extensions: the Direct projection rebuilt
    /// from the restored registry and the projection's own extension list is
    /// the projection that was persisted.
    #[test]
    fn hub_claims_recompile_exactly_over_the_restored_registry() {
        use crate::agent_tree_tools::TopologyVocabulary;
        use plakous::tools::CodingPresentationExt as _;
        for (presentation, vocabulary, claimed) in [
            (
                CodingPresentation::Grok,
                TopologyVocabulary::TaskTools,
                ["get_command_or_subagent_output", "kill_command_or_subagent"],
            ),
            (
                CodingPresentation::Pi,
                TopologyVocabulary::WaitStop,
                ["wait", "stop"],
            ),
        ] {
            let extensions = topology_extensions(vocabulary);
            let (live, projection) = hub_projection(presentation, &extensions);
            let restored = plakous::tools::FrozenToolRegistryDescriptor::decode_v2(
                &plakous::tools::FrozenToolRegistryDescriptor::capture(&live)
                    .expect("capture")
                    .encode_v2()
                    .expect("encode"),
            )
            .expect("decode")
            .resolve(&plakous::tools::ToolExecutorBindings::capture(&live))
            .expect("resolve");
            let extras = presentation
                .projection_extensions(
                    plakous::tools::JobConfig::default(),
                    restored.clone(),
                    &projection,
                )
                .expect("extensions");
            for name in claimed {
                assert!(
                    extras.iter().any(|extra| extra.as_str() == name),
                    "{presentation:?}: claimed {name} is an extension: {extras:?}"
                );
            }
            let recompiled = presentation
                .projection_map_with_extensions(
                    plakous::tools::JobConfig::default(),
                    restored,
                    projection.target().clone(),
                    &extras,
                )
                .expect("recompile over the restored registry");
            assert_eq!(recompiled.digest(), projection.digest(), "{presentation:?}");
        }
    }
}
