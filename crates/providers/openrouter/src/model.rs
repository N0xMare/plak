//! Dynamic Custom/experimental OpenRouter Responses target authority.

use std::collections::BTreeMap;
use std::fmt;

use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, Clock as _, CompiledProviderConfiguration,
    ContextContract, ContextEvidenceClass, Digest, DigestDomain, FactProvenance, FactSource,
    FeatureSupport, ModelAdapterProfileId, ModelFamilyId, ModelImplementationIdentity, NamespaceId,
    ProtocolId, ProviderSemanticEvidence, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, RuntimeModelAuthority, SelectedProviderConfiguration,
    Support, SystemClock, Timestamp,
};
use plakous_responses_dialect::{
    ResponsesProviderConfigurationError, ResponsesRouterPolicy,
    compile_provider_configuration_with_implementation_and_router_policy,
};
use thiserror::Error;

use crate::{
    OPENROUTER_RESPONSES_DIALECT_REVISION, OPENROUTER_RESPONSES_PATH,
    OPENROUTER_RESPONSES_PROTOCOL_ID, OpenRouterApiKeyGenerationGrant, OpenRouterAuthError,
    OpenRouterChatModelEvidence, OpenRouterChatRoute,
};

/// Custom Responses integration-profile identity.
pub const OPENROUTER_RESPONSES_PROFILE: &str = "openrouter.responses.dynamic-custom-v1";
/// Custom implementation namespace retained by configured targets.
pub const OPENROUTER_RESPONSES_IMPLEMENTATION: &str = "openrouter.responses.custom";
/// Custom implementation provenance namespace.
pub const OPENROUTER_RESPONSES_PROVENANCE: &str = "openrouter.responses.experimental";

/// Complete routing disposition for the currently admitted Responses target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub enum OpenRouterResponsesRoutingPolicy {
    /// OpenRouter selects the serving endpoint; Plakous claims no endpoint pin.
    GatewayManaged,
}

impl OpenRouterResponsesRoutingPolicy {
    /// Reports that the target can never select another wire family after admission.
    #[must_use]
    pub const fn allows_cross_protocol_fallback(self) -> bool {
        false
    }

    /// Reports that no endpoint/provider identity is claimed by this policy.
    #[must_use]
    pub const fn endpoint_claim(self) -> Option<&'static str> {
        None
    }
}

/// Immutable dynamic OpenRouter Responses target.
///
/// Authenticated literal discovery constrains the target but does not mint a
/// static model catalog, Standard support, context capacity, or endpoint pin.
#[derive(Clone)]
pub struct OpenRouterResponsesTarget {
    route: OpenRouterChatRoute,
    evidence: OpenRouterChatModelEvidence,
    context_contract: ContextContract,
    descriptor: ResolvedModelDescriptor,
    target_identity: Digest,
    discovery_identity: Digest,
}

impl OpenRouterResponsesTarget {
    /// Builds one GatewayManaged Custom/experimental Responses target.
    ///
    /// The same authenticated literal facts and explicit caller-owned Custom
    /// context envelope used by the Chat dialect are required here.
    pub fn gateway_managed(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
    ) -> Result<Self, OpenRouterResponsesBuildError> {
        if !evidence.validates(route.identity(), SystemClock.now()) {
            return Err(OpenRouterResponsesBuildError::StaleEvidence);
        }
        if !matches!(
            context_contract.capacity().evidence_class(),
            ContextEvidenceClass::CustomEnvelope { .. }
        ) || context_contract.protocol_family().as_str() != "responses-family"
            || !crate::chat::context_within_discovery(&context_contract, evidence.context_length())
        {
            return Err(OpenRouterResponsesBuildError::InvalidContextContract);
        }
        let discovery_identity =
            evidence.admission_identity(crate::discovery::AdmissionWire::Responses)?;
        let protocol = ProtocolId::new(OPENROUTER_RESPONSES_PROTOCOL_ID)?;
        let family = ModelFamilyId::new("openrouter.responses")?;
        let profile = ModelAdapterProfileId::new(OPENROUTER_RESPONSES_PROFILE)?;
        let descriptor = ResolvedModelDescriptor::new_custom(
            protocol,
            route.identity().route_id().clone(),
            evidence.model().clone(),
            None,
            Some(family),
            profile,
            responses_capabilities()?,
        )
        .with_context_contract(context_contract.clone());
        let dialect_revision = OPENROUTER_RESPONSES_DIALECT_REVISION.to_be_bytes();
        let target_identity = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-openrouter/responses-target-v1",
                OPENROUTER_RESPONSES_PATH.as_bytes(),
                OPENROUTER_RESPONSES_PROTOCOL_ID.as_bytes(),
                &dialect_revision,
                route.identity().fingerprint().as_bytes(),
                &discovery_identity.identity_bytes(),
                &context_contract.semantic_identity().identity_bytes(),
                b"routing=gateway-managed;endpoint-claim=none;cross-protocol-fallback=false",
            ],
        )?;
        Ok(Self {
            route,
            evidence,
            context_contract,
            descriptor,
            target_identity,
            discovery_identity,
        })
    }

    /// Returns the exact Custom descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Returns the literal model preserved from authenticated discovery.
    #[must_use]
    pub const fn model(&self) -> &plakous_model::ModelId {
        self.evidence.model()
    }

    /// Returns the exact credential-free target identity.
    #[must_use]
    pub const fn target_identity(&self) -> Digest {
        self.target_identity
    }

    /// Returns GatewayManaged routing with no endpoint claim.
    #[must_use]
    pub const fn routing(&self) -> OpenRouterResponsesRoutingPolicy {
        OpenRouterResponsesRoutingPolicy::GatewayManaged
    }

    pub(crate) const fn route(&self) -> &OpenRouterChatRoute {
        &self.route
    }

    pub(crate) fn runtime_authority(
        &self,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, OpenRouterResponsesBuildError> {
        if checked_at >= self.evidence.expires_at() {
            return Err(OpenRouterResponsesBuildError::StaleEvidence);
        }
        if !self.evidence.validates(self.route.identity(), checked_at)
            || self
                .evidence
                .admission_identity(crate::discovery::AdmissionWire::Responses)?
                != self.discovery_identity
            || self.context_contract.protocol_family().as_str() != "responses-family"
            || !crate::chat::context_within_discovery(
                &self.context_contract,
                self.evidence.context_length(),
            )
        {
            return Err(OpenRouterResponsesBuildError::ForeignEvidence);
        }
        let grant = self.route.current_credential_generation_grant()?;
        RuntimeModelAuthority::new(
            &self.descriptor,
            self.route.identity().account_binding(),
            self.route.identity().credential_authority_binding(),
            generation_number(grant),
            checked_at,
            Some(self.evidence.expires_at()),
        )
        .map_err(|_| OpenRouterResponsesBuildError::Configuration)
    }
}

impl fmt::Debug for OpenRouterResponsesTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterResponsesTarget")
            .field("route", self.route.identity())
            .field("model", &self.evidence.model())
            .field("routing", &OpenRouterResponsesRoutingPolicy::GatewayManaged)
            .field("descriptor", &self.descriptor)
            .field("target_identity", &self.target_identity)
            .finish_non_exhaustive()
    }
}

fn responses_capabilities() -> Result<CapabilitySet, plakous_model::IdentityError> {
    let provenance = FactProvenance::new(
        FactSource::BuiltIn,
        Timestamp::new(crate::chat::OPENROUTER_CHAT_REVIEWED_AT, 0)?,
        Some(Timestamp::new(
            crate::chat::OPENROUTER_CHAT_REVIEW_EXPIRES_AT,
            0,
        )?),
    );
    let features = [Capability::Streaming, Capability::Tools]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::new(
                        None,
                        Some(
                            "Custom OpenRouter Responses grammar; GatewayManaged requires all emitted parameters"
                                .to_owned(),
                        ),
                    ),
                    provenance,
                ),
            )
        })
        .collect();
    Ok(CapabilitySet::new(features, BTreeMap::new()))
}

fn generation_number(grant: OpenRouterApiKeyGenerationGrant) -> u64 {
    let fingerprint = grant.fingerprint();
    let bytes = fingerprint.as_bytes();
    u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])).max(1)
}

/// Compiles one dynamic Custom Responses selection with GatewayManaged routing.
pub fn compile_openrouter_responses_configuration(
    selected: &SelectedProviderConfiguration,
    target: &OpenRouterResponsesTarget,
    runtime: &RuntimeModelAuthority,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    let descriptor = target.descriptor();
    let implementation = ModelImplementationIdentity::custom(
        NamespaceId::new(OPENROUTER_RESPONSES_IMPLEMENTATION)
            .map_err(|_| ResponsesProviderConfigurationError::Identity)?,
        1,
        NamespaceId::new(OPENROUTER_RESPONSES_PROVENANCE)
            .map_err(|_| ResponsesProviderConfigurationError::Identity)?,
    )?;
    let semantic = ProviderSemanticEvidence::new(
        1,
        serde_json::to_vec(&(
            "plakous-openrouter/responses-semantic-v1",
            OPENROUTER_RESPONSES_PATH,
            OPENROUTER_RESPONSES_PROTOCOL_ID,
            OPENROUTER_RESPONSES_DIALECT_REVISION,
            target.target_identity(),
            "routing=gateway-managed;endpoint-claim=none;cross-protocol-fallback=false",
        ))
        .map_err(|_| ResponsesProviderConfigurationError::Identity)?,
    )?;
    let authority = ResolvedProviderAuthorityBinding::new(descriptor, runtime, semantic)?;
    let retention = target.route().identity().retention_policy().ok_or(
        ResponsesProviderConfigurationError::PolicyMismatch("retention_policy"),
    )?;
    let router = ResponsesRouterPolicy::gateway_managed(
        retention.data_collection().map(str::to_owned),
        retention.zdr(),
    )
    .map_err(|_| ResponsesProviderConfigurationError::Identity)?;
    compile_provider_configuration_with_implementation_and_router_policy(
        selected,
        descriptor.clone(),
        implementation,
        authority,
        crate::OpenRouterDialect::ResponsesV1.request_policy(),
        Some(&router),
    )
}

/// Safe dynamic Responses target-construction failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenRouterResponsesBuildError {
    /// Authenticated discovery evidence expired.
    #[error("OpenRouter Responses discovery evidence is stale")]
    StaleEvidence,
    /// Discovery/context reconstruction no longer names the same target.
    #[error("OpenRouter Responses discovery evidence is foreign")]
    ForeignEvidence,
    /// Caller context is not a bounded Responses-family Custom envelope.
    #[error("OpenRouter Responses requires a bounded Responses-family Custom context contract")]
    InvalidContextContract,
    /// Shared Chat discovery/context validation rejected the facts.
    #[error(transparent)]
    Discovery(#[from] crate::OpenRouterChatBuildError),
    /// Route credential authority was unavailable.
    #[error(transparent)]
    Authentication(#[from] OpenRouterAuthError),
    /// Identity construction failed.
    #[error(transparent)]
    Identity(#[from] plakous_model::IdentityError),
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] plakous_model::DigestError),
    /// Runtime authority construction failed closed.
    #[error("OpenRouter Responses runtime configuration is invalid")]
    Configuration,
    /// Selected or configured provider behavior was inconsistent.
    #[error(transparent)]
    ProviderConfiguration(#[from] plakous_model::ProviderConfigurationError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use plakous_model::{
        CapacityCertificate, CapacityRule, Clock, ContextEvidenceSource, CountCertificateKind,
        EvidenceFreshness, IntegrationClass, MeasurementCertificate, RuntimeModelAuthority,
        SystemClock, Timestamp,
    };

    use super::*;
    use crate::{
        OpenRouterApiKey, OpenRouterChatEndpointCandidate, OpenRouterOrigin,
        OpenRouterToolChoiceModes, StaticOpenRouterApiKeySource,
    };

    fn route() -> Result<OpenRouterChatRoute, Box<dyn std::error::Error>> {
        let source = Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("responses-target-test-key")?,
            "responses-target-account",
            "responses-target-authority",
        )?);
        Ok(OpenRouterChatRoute::new_local_fixture(
            source,
            OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        )?)
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
        let now = SystemClock.now();
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
                endpoints.clone(),
                BTreeSet::from(["tools".to_owned()]),
                None,
                4096,
                Timestamp::new(
                    now.seconds().saturating_sub(observed_offset),
                    now.nanoseconds(),
                )?,
                Timestamp::new(
                    now.seconds().saturating_add(expiry_offset),
                    now.nanoseconds(),
                )?,
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
}
