//! Exact Platform credentials, selection and caller-declared context authority.

use crate::{
    XAI_PLATFORM_API_KEY_AUTH_PROFILE, XAI_PLATFORM_API_KEY_AUTH_REVISION,
    XAI_PLATFORM_API_KEY_AUTHORITY_CLASS, XAI_PLATFORM_API_KEY_CREDENTIAL_SELECTION,
    XAI_PLATFORM_GROK_46_MODEL, XaiDialect, XaiPlatformApiKeyAuthority, XaiRouteKind,
};
use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule,
    ContextContract, ContextEvidenceSource, CountCertificateKind, CredentialSelectionId, Digest,
    DigestDomain, EvidenceFreshness, FactProvenance, FactSource, FeatureSupport,
    MeasurementCertificate, ModelAdapterProfileId, ModelFamilyId, ModelId, ProtocolId,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ResolvedModelDescriptor, RouteId, RuntimeModelAuthority,
    SelectedProviderConfiguration, Support, Timestamp,
};
use plakous_responses_dialect::{CredentialGenerationFingerprint, CredentialProviderTag};
use std::{collections::BTreeMap, fmt, num::NonZeroU64};
use zeroize::Zeroizing;

/// Executable Grok 4.6 local-tool profile; context is a caller declaration.
pub const XAI_PLATFORM_GROK_46_PROFILE: &str = "xai.platform.grok-4.6.custom-envelope.v1";
/// Exact reviewed executable Platform policy revision.
pub const XAI_PLATFORM_EXECUTION_REVISION: &str = "xai-platform-responses-2026-09-14-v1";
pub(crate) const PROJECTION: &str = "responses-canonical-xai-platform-v1";
pub(crate) const ACCOUNTING: &str = "responses-wire-bytes-xai-platform-v1";
pub(crate) const REVIEWED: u64 = 1_789_344_000;
pub(crate) const EXPIRES: u64 = REVIEWED + 90 * 86_400;

/// Redaction-safe Platform construction or credential-source failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum XaiPlatformBuildError {
    /// The exact model, effort, route, context or selected behavior is invalid.
    #[error("xAI Platform configuration is invalid")]
    Configuration,
    /// The reviewed route facts are no longer current.
    #[error("xAI Platform reviewed route facts are stale")]
    Stale,
    /// The current credential grant is absent, revoked, or changed.
    #[error("xAI Platform credential authority is unavailable or changed")]
    Authentication,
    /// Shared transport or Responses construction failed.
    #[error("xAI Platform Responses transport binding is invalid")]
    Transport,
}

/// Zeroized API key; debug formatting never reveals key material.
pub struct XaiPlatformApiKey(Zeroizing<String>);
impl XaiPlatformApiKey {
    /// Validates a nonempty bounded bearer without whitespace or control bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, XaiPlatformBuildError> {
        let value = Zeroizing::new(value.into());
        if value.is_empty() || value.len() > 16_384 || !value.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(XaiPlatformBuildError::Authentication);
        }
        Ok(Self(value))
    }
    /// Computes a non-secret exact grant for this key and source-owned generation.
    pub fn grant(
        &self,
        authority: XaiPlatformApiKeyAuthority,
    ) -> Result<XaiPlatformCredentialGrant, XaiPlatformBuildError> {
        let fingerprint = CredentialGenerationFingerprint::mint_exact(
            CredentialProviderTag::XaiPlatform,
            self.0.as_bytes(),
        )
        .map_err(|_| XaiPlatformBuildError::Authentication)?;
        Ok(XaiPlatformCredentialGrant {
            authority,
            fingerprint,
        })
    }
    pub(crate) fn bearer(&self) -> Result<plakous_http::HeaderValue, XaiPlatformBuildError> {
        plakous_http::sensitive_header_value(format!("Bearer {}", self.0.as_str()).into_bytes())
            .map_err(|_| XaiPlatformBuildError::Authentication)
    }
}
impl fmt::Debug for XaiPlatformApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("XaiPlatformApiKey(<redacted>)")
    }
}

/// Exact non-secret grant binding an API key to one account and generation.
#[derive(Clone, Eq, PartialEq)]
pub struct XaiPlatformCredentialGrant {
    pub(crate) authority: XaiPlatformApiKeyAuthority,
    pub(crate) fingerprint: CredentialGenerationFingerprint,
}
impl XaiPlatformCredentialGrant {
    pub(crate) fn execution_authority(
        &self,
    ) -> Result<XaiPlatformApiKeyAuthority, XaiPlatformBuildError> {
        self.authority
            .for_key_generation(self.fingerprint.as_bytes())
            .map_err(|_| XaiPlatformBuildError::Authentication)
    }
    /// Returns the exact source-owned account and generation.
    #[must_use]
    pub const fn authority(&self) -> &XaiPlatformApiKeyAuthority {
        &self.authority
    }
}
impl fmt::Debug for XaiPlatformCredentialGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("XaiPlatformCredentialGrant(<redacted>)")
    }
}

/// Application-owned credentials exclusively for `https://api.x.ai/v1/responses`.
/// No OAuth or alternate-origin resolution is permitted by this source contract.
pub trait XaiPlatformApiKeySource: Send + Sync {
    /// Returns the current non-secret grant without exposing bearer material.
    fn grant(&self) -> Result<XaiPlatformCredentialGrant, XaiPlatformBuildError>;
    /// Resolves the bearer only while the supplied exact grant remains current.
    fn api_key(
        &self,
        expected: &XaiPlatformCredentialGrant,
    ) -> Result<XaiPlatformApiKey, XaiPlatformBuildError>;
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

pub(crate) fn current(now: Timestamp) -> Result<(), XaiPlatformBuildError> {
    if now < timestamp(REVIEWED)? || now >= timestamp(EXPIRES)? {
        return Err(XaiPlatformBuildError::Stale);
    }
    Ok(())
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
        let expires = timestamp(EXPIRES)?;
        let provenance = FactProvenance::new(FactSource::BuiltIn, observed, Some(expires));
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
        let freshness = EvidenceFreshness::new(observed, Some(expires))?;
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
