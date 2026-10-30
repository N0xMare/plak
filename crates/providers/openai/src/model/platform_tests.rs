use std::sync::Arc;

use plakous_model::{Capability, CapacityRule, FixedClock, Timestamp};

use super::{OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts};
use crate::{
    CredentialAuthorityId, CredentialReference, EndpointOrigin, OPENAI_PLATFORM_ORIGIN,
    OpenAiPlatformRouteConfig, OpenAiProjectId, PlatformProjectPolicy, PlatformStoragePolicy,
    SecretString, StaticPlatformCredentialSource,
    manifest::OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS,
};

fn source(reference: &CredentialReference) -> Arc<StaticPlatformCredentialSource> {
    Arc::new(StaticPlatformCredentialSource::new(
        SecretString::new("fixture-key-never-dispatched").expect("fixture secret"),
        reference.clone(),
        CredentialAuthorityId::new("openai-platform-fixture-authority").expect("fixture authority"),
    ))
}

#[test]
fn token_window_is_installed_on_a_wire_byte_envelope() {
    let reference =
        CredentialReference::application("openai-platform-fixture").expect("fixture reference");
    let clock = Arc::new(FixedClock::new(
        Timestamp::new(
            OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS + 1,
            0,
        )
        .expect("fixture time"),
    ));
    let tokens = std::num::NonZeroU64::new(272_000).expect("window");
    let sealed = OpenAiPlatformModelBuilder::new(
        reference.clone(),
        source(&reference),
        OpenAiPlatformModelFacts::Gpt6Luna,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .clock(clock)
    .context_envelope(tokens)
    .seal()
    .expect("sealed authority");
    let contract = sealed
        .descriptor()
        .context_contract()
        .expect("caller envelope");
    assert_eq!(
        contract.accounting_revision(),
        "responses-wire-bytes-openai-platform-v1"
    );
    let measurement = contract
        .measurements()
        .iter()
        .find(|measurement| measurement.certificate_id() == "openai-platform-complete-wire-bytes")
        .expect("wire-byte measurement");
    assert_eq!(
        measurement.method(),
        "complete-lowered-wire-byte-upper-bound"
    );
    let CapacityRule::ProviderReservedInput {
        maximum_projected_input,
        ..
    } = contract.capacity().rule()
    else {
        panic!("envelope rule");
    };
    assert_eq!(
        maximum_projected_input, 272_000,
        "the token window is passed through; it is not converted to bytes"
    );
}

#[test]
fn literal_platform_models_are_closed_without_hosted_tool_claims() {
    let reference =
        CredentialReference::application("openai-platform-fixture").expect("fixture reference");
    let clock = Arc::new(FixedClock::new(
        Timestamp::new(
            OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS + 1,
            0,
        )
        .expect("fixture time"),
    ));
    for facts in [
        OpenAiPlatformModelFacts::Gpt6Sol,
        OpenAiPlatformModelFacts::Gpt6Luna,
        OpenAiPlatformModelFacts::Gpt56Sol,
        OpenAiPlatformModelFacts::Gpt56Terra,
        OpenAiPlatformModelFacts::Gpt56Luna,
    ] {
        let sealed = OpenAiPlatformModelBuilder::new(
            reference.clone(),
            source(&reference),
            facts,
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .clock(clock.clone())
        .seal()
        .expect("sealed local authority");
        assert_eq!(
            sealed.descriptor().requested_model().as_str(),
            facts.model_id()
        );
        assert_eq!(sealed.project(), &PlatformProjectPolicy::DefaultProject);
        assert_eq!(
            sealed
                .descriptor()
                .capabilities()
                .get(Capability::HostedProgrammaticToolCalling)
                .map(plakous_model::FeatureSupport::support),
            None
        );
    }
}

#[test]
fn project_and_storage_are_explicit_serialized_route_authority() {
    let origin = EndpointOrigin::parse(OPENAI_PLATFORM_ORIGIN).expect("official origin");
    let reference =
        CredentialReference::application("openai-platform-fixture").expect("fixture reference");
    let default = OpenAiPlatformRouteConfig::new(
        origin.clone(),
        reference.clone(),
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .expect("default-project route");
    let project = OpenAiPlatformRouteConfig::new(
        origin,
        reference,
        PlatformStoragePolicy::StoreResponses,
        PlatformProjectPolicy::Project(OpenAiProjectId::new("proj_fixture").expect("project ID")),
    )
    .expect("project route");
    assert_ne!(default.account_binding(), project.account_binding());
    let encoded = serde_json::to_value(&project).expect("serialize route");
    assert_eq!(encoded["storage"], "store_responses");
    assert_eq!(encoded["project"]["project"], "proj_fixture");
    let reopened: OpenAiPlatformRouteConfig =
        serde_json::from_value(encoded).expect("deserialize exact route");
    assert_eq!(reopened, project);
}

#[test]
fn project_identifier_rejects_ambiguous_header_values() {
    assert!(OpenAiProjectId::new("").is_err());
    assert!(OpenAiProjectId::new("proj_é").is_err());
    assert!(OpenAiProjectId::new("proj_fixture\nother").is_err());
    assert!(OpenAiProjectId::new("x".repeat(257)).is_err());
}
