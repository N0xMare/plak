#![allow(clippy::expect_used, reason = "unit tests may assert with expect")]

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
            assert_eq!(
                crate::binding::admit_account_model(provider.account(), &row.model),
                Ok(()),
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
        selected_route_presentation(NativeProvider::Claude, GROK_PROCESS_PROJECTION_MODEL).is_err()
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
