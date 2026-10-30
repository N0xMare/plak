//! Exact Platform credentials, selection and caller-declared context authority.

use crate::{
    XAI_PLATFORM_API_KEY_AUTH_PROFILE, XAI_PLATFORM_API_KEY_AUTH_REVISION,
    XAI_PLATFORM_API_KEY_AUTHORITY_CLASS, XAI_PLATFORM_API_KEY_CREDENTIAL_SELECTION,
    XAI_PLATFORM_GROK_46_MODEL, XaiDialect, XaiPlatformApiKeyAuthority, XaiRouteKind,
};
pub use plakous_auth::api_key::ApiKey;
use plakous_auth::api_key::CredentialProviderTag;
use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule,
    ContextContract, ContextEvidenceSource, CountCertificateKind, CredentialSelectionId, Digest,
    DigestDomain, EvidenceFreshness, FactProvenance, FactSource, FeatureSupport,
    MeasurementCertificate, ModelAdapterProfileId, ModelFamilyId, ModelId, ProtocolId,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ResolvedModelDescriptor, RouteId, RuntimeModelAuthority,
    SelectedProviderConfiguration, Support, Timestamp,
};
use std::{collections::BTreeMap, num::NonZeroU64};

/// Executable Grok 4.6 local-tool profile; context is a caller declaration.
pub const XAI_PLATFORM_GROK_46_PROFILE: &str = "xai.platform.grok-4.6.custom-envelope.v1";
/// Exact reviewed executable Platform policy revision.
pub const XAI_PLATFORM_EXECUTION_REVISION: &str = "xai-platform-responses-2026-09-14-v1";
pub(crate) const PROJECTION: &str = "responses-canonical-xai-platform-v1";
pub(crate) const ACCOUNTING: &str = "responses-wire-bytes-xai-platform-v1";
// Reviewed 2026-10-30: docs.x.ai/developers/models/grok-4.6,
// developers/model-capabilities/text/generate-text and developers/tools/function-calling.
pub(crate) const REVIEWED: u64 = 1_793_318_400;
// Drives only doctor and status notes and the publish floor; the route never
// refuses on the calendar.
pub(crate) const REVIEW_DUE: u64 = REVIEWED + 90 * 86_400;

/// Redaction-safe Platform construction or credential-source failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum XaiPlatformBuildError {
    /// The exact model, effort, route, context or selected behavior is invalid.
    #[error("xAI Platform configuration is invalid")]
    Configuration,
    /// The current credential grant is absent, revoked, or changed.
    #[error("xAI Platform credential authority is unavailable or changed")]
    Authentication,
    /// Shared transport or Responses construction failed.
    #[error("xAI Platform Responses transport binding is invalid")]
    Transport,
}

impl From<plakous_auth::api_key::ApiKeyError> for XaiPlatformBuildError {
    fn from(_: plakous_auth::api_key::ApiKeyError) -> Self {
        Self::Authentication
    }
}

/// Source-owned xAI authority over the shared non-secret generation grant.
pub type XaiPlatformKeyGeneration =
    plakous_auth::api_key::ApiKeyGenerationGrant<XaiPlatformApiKeyAuthority>;

/// Fixed xAI Platform credential audience, without an alternate-origin input.
pub struct XaiPlatformApiKeyRequest(());

impl XaiPlatformApiKeyRequest {
    /// Names the official Platform credential audience.
    #[must_use]
    pub(crate) const fn platform() -> Self {
        Self(())
    }
}

impl plakous_auth::api_key::ApiKeyRequest for XaiPlatformApiKeyRequest {
    fn provider(&self) -> CredentialProviderTag {
        CredentialProviderTag::XaiPlatform
    }
    fn origin(&self) -> &str {
        crate::platform::XAI_PLATFORM_API_KEY_ORIGIN
    }
}

pub(crate) const API_KEY_REQUEST: XaiPlatformApiKeyRequest = XaiPlatformApiKeyRequest::platform();

#[cfg(test)]
mod tests;

/// The common source over xAI's exact request and source-owned account authority.
pub type XaiPlatformKeySource =
    dyn plakous_auth::api_key::ApiKeySource<XaiPlatformApiKeyRequest, XaiPlatformApiKeyAuthority>;

/// Returns only the current non-secret xAI account and generation authority.
///
/// # Errors
/// Refuses unavailable or changed credential authority without resolving a bearer.
pub fn xai_api_key_generation(
    source: &XaiPlatformKeySource,
) -> Result<XaiPlatformKeyGeneration, XaiPlatformBuildError> {
    let grant = source
        .generation_grant(&API_KEY_REQUEST)
        .map_err(|_| XaiPlatformBuildError::Authentication)?;
    if grant.provider() != CredentialProviderTag::XaiPlatform
        || grant.authority() != source.authority()
    {
        return Err(XaiPlatformBuildError::Authentication);
    }
    Ok(grant)
}

pub(crate) fn execution_authority(
    grant: &XaiPlatformKeyGeneration,
) -> Result<XaiPlatformApiKeyAuthority, XaiPlatformBuildError> {
    grant
        .authority()
        .for_key_generation(grant.fingerprint().as_bytes())
        .map_err(|_| XaiPlatformBuildError::Authentication)
}

pub(crate) fn platform_bearer(
    key: &ApiKey,
) -> Result<plakous_http::HeaderValue, XaiPlatformBuildError> {
    use plakous_auth::api_key::ExposeApiKey as _;
    let text = key.expose_secret();
    if text.len() > 16_384 || !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(XaiPlatformBuildError::Authentication);
    }
    plakous_http::sensitive_header_value(format!("Bearer {text}").into_bytes())
        .map_err(|_| XaiPlatformBuildError::Authentication)
}

/// Builds the grok-4.6 route. Other reviewed ids use
/// [`xai_platform_selection_for`] under the same platform profile.
pub fn xai_platform_selection(
    behavior: ProviderBehaviorConfiguration,
) -> Result<SelectedProviderConfiguration, XaiPlatformBuildError> {
    xai_platform_selection_for(XAI_PLATFORM_GROK_46_MODEL, behavior)
}

/// Builds the platform route for one reviewed model id. The profile stays the
/// existing custom-envelope policy; the requested model id is the catalog id.
pub fn xai_platform_selection_for(
    model: &str,
    behavior: ProviderBehaviorConfiguration,
) -> Result<SelectedProviderConfiguration, XaiPlatformBuildError> {
    // Every usable reviewed row publishes `low`. An id that does not is not
    // this profile.
    if !crate::xai_platform_effort_admitted(model, plakous_model::ReasoningEffort::Low) {
        return Err(XaiPlatformBuildError::Configuration);
    }
    let build = || -> Result<_, Box<dyn std::error::Error>> {
        Ok(SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                ProtocolId::new(XaiDialect::PlatformResponses2026_08.protocol_id())?,
                RouteId::new(XaiRouteKind::PlatformApiKey.route_id())?,
                ModelAdapterProfileId::new(XAI_PLATFORM_GROK_46_PROFILE)?,
                ProviderProfileRevisionId::new(XAI_PLATFORM_EXECUTION_REVISION)?,
                ModelAdapterProfileId::new(XAI_PLATFORM_API_KEY_AUTH_PROFILE)?,
                ProviderProfileRevisionId::new(XAI_PLATFORM_API_KEY_AUTH_REVISION)?,
                ProviderAuthorityClassId::new(XAI_PLATFORM_API_KEY_AUTHORITY_CLASS)?,
                CredentialSelectionId::new(XAI_PLATFORM_API_KEY_CREDENTIAL_SELECTION)?,
                ModelId::new(model)?,
            ),
            behavior,
        )?)
    };
    build().map_err(|_| XaiPlatformBuildError::Configuration)
}

fn timestamp(seconds: u64) -> Result<Timestamp, XaiPlatformBuildError> {
    Timestamp::new(seconds, 0).map_err(|_| XaiPlatformBuildError::Configuration)
}

pub(crate) fn descriptor(
    authority: &XaiPlatformApiKeyAuthority,
    model: &str,
    maximum: NonZeroU64,
) -> Result<ResolvedModelDescriptor, XaiPlatformBuildError> {
    let build = || -> Result<_, Box<dyn std::error::Error>> {
        let observed = timestamp(REVIEWED)?;
        let provenance = FactProvenance::new(FactSource::BuiltIn, observed, None);
        // Every usable reviewed row lists `image` among its input modalities
        // in `GET /v1/language-models`.
        let features = [
            Capability::Streaming,
            Capability::Tools,
            Capability::Reasoning,
            Capability::ImageInput,
        ]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
        let model = ModelId::new(model)?;
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new(XaiDialect::PlatformResponses2026_08.protocol_id())?,
            RouteId::new(XaiRouteKind::PlatformApiKey.route_id())?,
            model.clone(),
            Some(model),
            Some(ModelFamilyId::new("grok")?),
            ModelAdapterProfileId::new(XAI_PLATFORM_GROK_46_PROFILE)?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let binding = RuntimeModelAuthority::authority_class_for(
            authority.account_binding(),
            authority.credential_authority_binding(),
        )?;
        let declaration = Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[
                b"xai-platform-caller-wire-envelope-v1",
                &maximum.get().to_be_bytes(),
            ],
        )?;
        let freshness = EvidenceFreshness::new(observed, None)?;
        let capacity = CapacityCertificate::custom_envelope(
            "xai-platform-caller-input-envelope",
            "v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: maximum.get(),
                auto_compact_input: None,
            },
            binding,
            freshness,
            declaration,
            "v1",
        )?;
        let measurement = MeasurementCertificate::new(
            "xai-platform-complete-wire-bytes",
            "v1",
            "complete-lowered-wire-byte-upper-bound",
            "v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(binding),
            freshness,
        )?;
        Ok(descriptor.with_context_contract(ContextContract::new(
            ProtocolId::new("responses-family")?,
            ACCOUNTING,
            PROJECTION,
            "xai-platform-output-reservation-v1",
            capacity,
            vec![measurement],
        )?))
    };
    build().map_err(|_| XaiPlatformBuildError::Configuration)
}
