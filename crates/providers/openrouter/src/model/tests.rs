use std::collections::BTreeSet;
use std::sync::Arc;

use plakous_model::{
    CapacityCertificate, CapacityRule, Clock, ContextEvidenceSource, CountCertificateKind,
    EvidenceFreshness, IntegrationClass, MeasurementCertificate, RuntimeModelAuthority, Timestamp,
};

use super::*;
use crate::{ApiKey, OpenRouterChatEndpointCandidate, OpenRouterOrigin, OpenRouterToolChoiceModes};

fn route() -> Result<OpenRouterChatRoute, Box<dyn std::error::Error>> {
    let source = Arc::new(crate::static_openrouter_api_key_source(
        ApiKey::new("responses-target-test-key")?,
        "responses-target-account",
        "responses-target-authority",
    )?);
    Ok(OpenRouterChatRoute::new_local_fixture(
        source,
        OpenRouterOrigin::new("https://127.0.0.1:18443")?,
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?))))
}

fn context(route: &OpenRouterChatRoute) -> Result<ContextContract, Box<dyn std::error::Error>> {
    let authority = RuntimeModelAuthority::authority_class_for(
        route.identity().account_binding(),
        route.identity().credential_authority_binding(),
    )?;
    let declaration = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[b"caller/openrouter-responses-context-v1"],
    )?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
    let capacity = CapacityCertificate::custom_envelope(
        "caller-openrouter-responses-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(4096),
            maximum_projected_input: 2048,
            auto_compact_input: None,
        },
        authority,
        freshness,
        declaration,
        "v1",
    )?;
    let measurement = MeasurementCertificate::new(
        "caller-openrouter-responses-wire",
        "v1",
        "complete-lowered-responses-body-utf8-byte-upper-bound",
        "v1",
        ContextEvidenceSource::CallerDeclaration,
        CountCertificateKind::CustomEnvelopeUpperBound { declaration },
        Some(authority),
        freshness,
    )?;
    Ok(ContextContract::new(
        ProtocolId::new("responses-family")?,
        "openrouter-responses-custom-accounting-v1",
        "openrouter-responses-canonical-v1",
        "openrouter-responses-output-reservation-v1",
        capacity,
        vec![measurement],
    )?)
}

#[test]
fn time_only_refresh_preserves_dynamic_custom_gateway_target()
-> Result<(), Box<dyn std::error::Error>> {
    let route = route()?;
    let now = plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)
    .now();
    let model = plakous_model::ModelId::new("vendor/dynamic-model")?;
    let endpoints = vec![OpenRouterChatEndpointCandidate {
        endpoint: "provider/dynamic-model".to_owned(),
        provider_name: "Provider".to_owned(),
        supported_parameters: BTreeSet::from(["tools".to_owned()]),
        context_length: Some(4096),
        max_prompt_tokens: Some(3072),
        max_completion_tokens: Some(1024),
        tool_choice: OpenRouterToolChoiceModes::default(),
        supports_implicit_caching: false,
    }];
    let evidence = |observed_offset: u64, expiry_offset: u64| {
        OpenRouterChatModelEvidence::from_authenticated_discovery(
            model.clone(),
            route.identity(),
            crate::chat::OpenRouterChatDiscovery {
                endpoints: endpoints.clone(),
                model_supported_parameters: BTreeSet::from(["tools".to_owned()]),
                reasoning: None,
                context_length: 4096,
                image_input: false,
                observed_at: Timestamp::new(
                    now.seconds().saturating_sub(observed_offset),
                    now.nanoseconds(),
                )?,
                expires_at: Timestamp::new(
                    now.seconds().saturating_add(expiry_offset),
                    now.nanoseconds(),
                )?,
            },
        )
    };
    let first = OpenRouterResponsesTarget::gateway_managed(
        route.clone(),
        evidence(20, 3600)?,
        context(&route)?,
    )?;
    let second = OpenRouterResponsesTarget::gateway_managed(
        route.clone(),
        evidence(10, 7200)?,
        context(&route)?,
    )?;
    assert_eq!(first.target_identity(), second.target_identity());
    assert_eq!(first.descriptor(), second.descriptor());
    assert_eq!(
        first.descriptor().integration_class(),
        IntegrationClass::Custom
    );
    assert_eq!(first.routing().endpoint_claim(), None);
    assert!(!first.routing().allows_cross_protocol_fallback());
    Ok(())
}
