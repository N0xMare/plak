//! Trusted production model and descriptor resolution for Anthropic Platform.

use std::collections::BTreeMap;
use std::sync::Arc;

use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, Clock, CompiledProviderConfiguration,
    FactProvenance, FactSource, FeatureSupport, ModelAdapterProfileId, ModelFamilyId, ModelId,
    ProtocolId, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    SelectedProviderConfiguration, Support, SystemClock, Timestamp,
};

use crate::{
    AnthropicError, AnthropicErrorKind, AnthropicErrorPhase, AnthropicMessagesDialect,
    AnthropicModel, AnthropicPlatformRoute, ClaudeProfile, ResolvedAnthropicMessages,
    ThinkingSupport,
};

const MANUAL_THINKING_MINIMUM_TOKENS: u32 = 1_024;

/// Immutable revision of the crate-reviewed Anthropic Platform admission manifest.
pub(crate) const ANTHROPIC_PLATFORM_ADMISSION_REVISION: &str =
    "anthropic.platform.admission.2026-09-22.v3";
/// UTC review time of the immutable admission manifest.
pub(crate) const ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS: u64 = 1_793_318_400;
/// When the admission manifest is due for re-review, 90 days after its review.
///
/// It drives only doctor and status notes and the publish floor; the route
/// never refuses on the calendar.
pub(crate) const ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_SECONDS: u64 =
    ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS + 90 * 86_400;
/// Exact remote model ID admitted by the current reviewed manifest.
pub const CLAUDE_SONNET_4_6_MODEL_ID: &str = "claude-sonnet-4-6";
/// Exact family identity admitted for [`CLAUDE_SONNET_4_6_MODEL_ID`].
pub(crate) const CLAUDE_SONNET_4_FAMILY_ID: &str = "claude-4-sonnet";
/// Conservative streaming/client-tools profile admitted by the current manifest.
pub(crate) const CLAUDE_SONNET_4_CLIENT_TOOLS_PROFILE_ID: &str =
    "anthropic.messages.client-tools.v1";
/// Streaming/client-tools/manual-thinking profile admitted by the current manifest.
pub(crate) const CLAUDE_SONNET_4_CLIENT_TOOLS_THINKING_PROFILE_ID: &str =
    "anthropic.messages.client-tools-thinking.v1";
/// Exact pinned Claude Sonnet 5 API model ID admitted for offline preparation.
pub const CLAUDE_SONNET_5_MODEL_ID: &str = "claude-sonnet-5";
/// Exact family identity admitted for [`CLAUDE_SONNET_5_MODEL_ID`].
pub(crate) const CLAUDE_SONNET_5_FAMILY_ID: &str = "claude-5-sonnet";
/// Conservative Sonnet 5 profile: streaming text/client tools with thinking disabled.
pub(crate) const CLAUDE_SONNET_5_CLIENT_TOOLS_PROFILE_ID: &str =
    "anthropic.messages.sonnet-5-client-tools.v1";
/// Sonnet 5 Platform profile for exact hosted programmatic tool calling.
pub const CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID: &str =
    "anthropic.messages.sonnet-5-hosted-ptc-20260120.v1";
/// Exact conservative Messages integration revision.
pub(crate) const ANTHROPIC_SONNET_5_INTEGRATION_REVISION: &str =
    "anthropic.messages.sonnet-5.2026-08-11.v1";
/// Exact direct API-key auth profile.
pub(crate) const ANTHROPIC_API_KEY_AUTH_PROFILE_ID: &str = "anthropic.platform.api-key";
/// Exact direct API-key auth revision.
pub(crate) const ANTHROPIC_API_KEY_AUTH_REVISION: &str = "anthropic.platform.api-key.v1";
/// Exact direct API-key authority class.
pub(crate) const ANTHROPIC_API_KEY_AUTHORITY_CLASS: &str = "anthropic.platform.api-key";
/// Exact explicit credential selection for this offline profile.
pub(crate) const ANTHROPIC_API_KEY_CREDENTIAL_SELECTION: &str =
    "anthropic.platform.explicit-api-key";

#[cfg(test)]
/// Exact Opus 5 Platform model identity.
pub(crate) const CLAUDE_OPUS_5_MODEL_ID: &str = "claude-opus-5";
#[cfg(test)]
/// Exact Opus 5.5 Platform model identity.
pub(crate) const CLAUDE_OPUS_5_5_MODEL_ID: &str = "claude-opus-5-5";
/// Stable API-key adaptive-thinking/local-tools integration profile.
pub const ANTHROPIC_API_ADAPTIVE_PROFILE_ID: &str = "anthropic.platform.adaptive-client-tools.v1";

const REVIEWED_MAXIMUM_OUTPUT_TOKENS: u32 = 8_192;
/// Enrolled Claude 5 rows take their output ceiling from the one Claude
/// enrollment table (the Models API `max_tokens`).
const CLAUDE_5_MAXIMUM_OUTPUT_TOKENS: u32 = crate::oauth::CLAUDE_ENROLLMENTS[0].max_output_tokens;

/// Compiles one exact conservative Sonnet 5 selection through the Messages kernel.
pub fn compile_anthropic_sonnet_5_configuration(
    selected: &SelectedProviderConfiguration,
    resolved: &ResolvedAnthropicMessages,
    authority: ResolvedProviderAuthorityBinding,
) -> Result<CompiledProviderConfiguration, AnthropicError> {
    let route = selected.route();
    let exact = route.protocol().as_str() == "anthropic.messages"
        && route.route().as_str() == "anthropic.platform"
        && route.integration_profile().as_str() == CLAUDE_SONNET_5_CLIENT_TOOLS_PROFILE_ID
        && route.integration_revision().as_str() == ANTHROPIC_SONNET_5_INTEGRATION_REVISION
        && route.auth_profile().as_str() == ANTHROPIC_API_KEY_AUTH_PROFILE_ID
        && route.auth_revision().as_str() == ANTHROPIC_API_KEY_AUTH_REVISION
        && route.authority_class().as_str() == ANTHROPIC_API_KEY_AUTHORITY_CLASS
        && route.credential_selection().as_str() == ANTHROPIC_API_KEY_CREDENTIAL_SELECTION
        && route.requested_model().as_str() == CLAUDE_SONNET_5_MODEL_ID
        && resolved.model().remote_id().as_str() == CLAUDE_SONNET_5_MODEL_ID
        && resolved.profile().id().as_str() == CLAUDE_SONNET_5_CLIENT_TOOLS_PROFILE_ID
        && resolved.dialect() == &AnthropicMessagesDialect::stable();
    if !exact {
        return Err(configuration_identity(
            "Anthropic selected provider configuration drifted",
        ));
    }
    plakous_messages::compile_messages_configuration(
        selected,
        resolved.descriptor().clone(),
        authority,
        crate::ANTHROPIC_API_VERSION,
        "",
    )
    .map_err(configuration_identity)
}

#[derive(Clone, Copy)]
struct AdmissionRecord {
    remote_id: &'static str,
    family: &'static str,
    profile: &'static str,
    capabilities: AnthropicPlatformCapabilityProfile,
    maximum_output_tokens: u32,
}

/// Exact admissions for the conservative profiles. The stable adaptive API
/// profile is admitted by the reviewed family rule instead (`api_models`).
const REVIEWED_ADMISSIONS: [AdmissionRecord; 4] = [
    AdmissionRecord {
        remote_id: CLAUDE_SONNET_4_6_MODEL_ID,
        family: CLAUDE_SONNET_4_FAMILY_ID,
        profile: CLAUDE_SONNET_4_CLIENT_TOOLS_PROFILE_ID,
        capabilities: AnthropicPlatformCapabilityProfile::TextAndClientTools,
        maximum_output_tokens: REVIEWED_MAXIMUM_OUTPUT_TOKENS,
    },
    AdmissionRecord {
        remote_id: CLAUDE_SONNET_4_6_MODEL_ID,
        family: CLAUDE_SONNET_4_FAMILY_ID,
        profile: CLAUDE_SONNET_4_CLIENT_TOOLS_THINKING_PROFILE_ID,
        capabilities: AnthropicPlatformCapabilityProfile::TextClientToolsAndThinking,
        maximum_output_tokens: REVIEWED_MAXIMUM_OUTPUT_TOKENS,
    },
    AdmissionRecord {
        remote_id: CLAUDE_SONNET_5_MODEL_ID,
        family: CLAUDE_SONNET_5_FAMILY_ID,
        profile: CLAUDE_SONNET_5_CLIENT_TOOLS_PROFILE_ID,
        capabilities: AnthropicPlatformCapabilityProfile::TextAndClientTools,
        maximum_output_tokens: CLAUDE_5_MAXIMUM_OUTPUT_TOKENS,
    },
    AdmissionRecord {
        remote_id: CLAUDE_SONNET_5_MODEL_ID,
        family: CLAUDE_SONNET_5_FAMILY_ID,
        profile: CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID,
        capabilities: AnthropicPlatformCapabilityProfile::HostedProgrammaticTools,
        maximum_output_tokens: CLAUDE_5_MAXIMUM_OUTPUT_TOKENS,
    },
];

#[derive(Clone, Copy)]
struct AdmissionEvidence {
    record: AdmissionRecord,
    revision: &'static str,
    source: FactSource,
    observed_at: Timestamp,
}

/// Private authority boundary for production admission facts.
///
/// The public resolver constructors deliberately expose no source-injection
/// hook. A future authenticated Models API catalog can implement this boundary
/// inside the crate and return bounded observations without accepting
/// caller-authored capability evidence.
trait AnthropicAdmissionSource: Send + Sync + 'static {
    fn admit(
        &self,
        config: &AnthropicPlatformModelConfig,
    ) -> Result<AdmissionEvidence, AnthropicError>;

    fn label(&self) -> &'static str;
}

#[derive(Clone, Copy, Debug, Default)]
struct ReviewedManifestAdmissionSource;

impl AnthropicAdmissionSource for ReviewedManifestAdmissionSource {
    fn admit(
        &self,
        config: &AnthropicPlatformModelConfig,
    ) -> Result<AdmissionEvidence, AnthropicError> {
        let record = reviewed_admission(config)?;
        let observed_at = admission_timestamp(ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS)?;
        Ok(AdmissionEvidence {
            record,
            revision: ANTHROPIC_PLATFORM_ADMISSION_REVISION,
            source: FactSource::BuiltIn,
            observed_at,
        })
    }

    fn label(&self) -> &'static str {
        "reviewed-built-in-manifest"
    }
}

/// Crate-owned conservative capability profile for a production Messages model.
///
/// This is intentionally not an arbitrary capability set. Callers select one
/// of the adapter profiles admitted for an exact model by the reviewed
/// manifest, and the resolver derives every neutral capability and provenance
/// fact from that record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AnthropicPlatformCapabilityProfile {
    /// Streaming text and one or more Plak-executed client tool calls.
    TextAndClientTools,
    /// Text/client tools plus manual thinking controls and signed replay.
    TextClientToolsAndThinking,
    /// Client tools plus exact provider-hosted Python programmatic composition.
    HostedProgrammaticTools,
    /// Stable adaptive thinking and local client tools.
    AdaptiveClientTools,
}

impl AnthropicPlatformCapabilityProfile {
    const fn thinking(self) -> ThinkingSupport {
        match self {
            Self::TextAndClientTools => ThinkingSupport::Disabled,
            Self::TextClientToolsAndThinking => ThinkingSupport::Manual {
                minimum_budget_tokens: MANUAL_THINKING_MINIMUM_TOKENS,
            },
            Self::HostedProgrammaticTools => ThinkingSupport::Disabled,
            Self::AdaptiveClientTools => ThinkingSupport::Adaptive,
        }
    }
}

/// Caller-supplied identities and safe limits checked by the production resolver.
///
/// No descriptor, provenance, freshness timestamp, support quality, or native
/// option namespace can be supplied through this type. Construction is not
/// admission: [`AnthropicPlatformModelResolver::resolve`] requires an exact
/// match in the immutable crate-reviewed manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicPlatformModelConfig {
    remote_id: ModelId,
    family: ModelFamilyId,
    maximum_output_tokens: u32,
    profile: ModelAdapterProfileId,
    capabilities: AnthropicPlatformCapabilityProfile,
    dialect: AnthropicMessagesDialect,
}

impl AnthropicPlatformModelConfig {
    /// Creates one exact production model selection.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error when the output-token ceiling is zero.
    pub fn new(
        remote_id: ModelId,
        family: ModelFamilyId,
        maximum_output_tokens: u32,
        profile: ModelAdapterProfileId,
        capabilities: AnthropicPlatformCapabilityProfile,
    ) -> Result<Self, AnthropicError> {
        if maximum_output_tokens == 0 {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Anthropic production model maximum output tokens must be nonzero",
            ));
        }
        Ok(Self {
            remote_id,
            family,
            maximum_output_tokens,
            profile,
            capabilities,
            dialect: AnthropicMessagesDialect::stable(),
        })
    }

    /// Replaces the exact API-version and explicit beta selection.
    #[must_use]
    pub fn with_dialect(mut self, dialect: AnthropicMessagesDialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Returns the exact remote model ID, without alias rewriting.
    #[must_use]
    pub const fn remote_id(&self) -> &ModelId {
        &self.remote_id
    }

    /// Returns the selected crate-owned capability profile.
    #[must_use]
    pub const fn capability_profile(&self) -> AnthropicPlatformCapabilityProfile {
        self.capabilities
    }
}

/// Resolver for a single immutable credential-bearing Anthropic Platform route.
///
/// The resolver owns the clock used for every admission, preparation, and start
/// freshness check. It derives a minimal descriptor only from the immutable,
/// bounded crate-reviewed manifest, so resolving a caller selection cannot mint
/// or renew capability evidence.
#[derive(Clone)]
pub struct AnthropicPlatformModelResolver {
    route: AnthropicPlatformRoute,
    clock: Arc<dyn Clock>,
    admission_source: Arc<dyn AnthropicAdmissionSource>,
}

pub(crate) fn preflight_platform_admission(
    config: &AnthropicPlatformModelConfig,
) -> Result<(), AnthropicError> {
    ReviewedManifestAdmissionSource.admit(config).map(|_| ())
}

impl AnthropicPlatformModelResolver {
    /// Creates a resolver using the system clock and the crate-owned freshness window.
    #[must_use]
    pub fn new(route: AnthropicPlatformRoute) -> Self {
        Self {
            route,
            clock: Arc::new(SystemClock),
            admission_source: Arc::new(ReviewedManifestAdmissionSource),
        }
    }

    /// Creates a resolver with explicit time ownership for deterministic embedding.
    ///
    /// The same clock is retained through the resolved target and resulting
    /// [`AnthropicLanguageModel`](crate::AnthropicLanguageModel). Callers cannot choose the reviewed evidence
    /// lifetime.
    #[must_use]
    pub fn with_runtime(route: AnthropicPlatformRoute, clock: Arc<dyn Clock>) -> Self {
        Self {
            route,
            clock,
            admission_source: Arc::new(ReviewedManifestAdmissionSource),
        }
    }

    /// Returns the exact secret-bearing route owned by this resolver.
    #[must_use]
    pub const fn route(&self) -> &AnthropicPlatformRoute {
        &self.route
    }

    /// Resolves an exact Messages binding from current reviewed capability facts.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error if the exact model/family/profile/
    /// capability tuple is absent, its configured ceiling exceeds the reviewed
    /// ceiling, the manifest is not currently valid, or the neutral descriptor
    /// cannot be represented safely.
    pub fn resolve(
        &self,
        config: AnthropicPlatformModelConfig,
    ) -> Result<ResolvedAnthropicMessages, AnthropicError> {
        let evidence = self.admission_source.admit(&config)?;
        let provenance = FactProvenance::new(evidence.source, evidence.observed_at, None);
        let capabilities = derived_capabilities(
            config.capabilities,
            config
                .maximum_output_tokens
                .min(evidence.record.maximum_output_tokens),
            provenance,
        );
        let protocol = ProtocolId::new("anthropic.messages").map_err(configuration_identity)?;
        let model = AnthropicModel::new(
            config.remote_id.clone(),
            config.family.clone(),
            config.maximum_output_tokens,
            config.capabilities.thinking(),
        )?;
        let profile = ClaudeProfile::new(config.profile.clone());
        let expected_reported_model = config.remote_id.clone();
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            protocol,
            self.route.id().clone(),
            config.remote_id,
            Some(expected_reported_model),
            Some(config.family),
            config.profile,
            capabilities,
        );
        ResolvedAnthropicMessages::new_reviewed_admission(
            config.dialect,
            self.route.clone(),
            model,
            profile,
            descriptor,
            crate::AnthropicResolutionTrust::ReviewedApiAdmission {
                revision: evidence.revision,
                reviewed_at: evidence.observed_at,
            },
            Arc::clone(&self.clock),
        )
    }
}

impl std::fmt::Debug for AnthropicPlatformModelResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicPlatformModelResolver")
            .field("route", &self.route)
            .field("clock", &"<clock>")
            .field("admission_source", &self.admission_source.label())
            .finish()
    }
}

fn derived_capabilities(
    profile: AnthropicPlatformCapabilityProfile,
    maximum_output_tokens: u32,
    provenance: FactProvenance,
) -> CapabilitySet {
    let native = |maximum: Option<u64>, feature: &str| {
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::new(
                maximum,
                Some(format!("{ANTHROPIC_PLATFORM_ADMISSION_REVISION}:{feature}")),
            ),
            provenance,
        )
    };
    let mut features = BTreeMap::new();
    features.insert(Capability::Streaming, native(None, "streaming"));
    features.insert(Capability::Tools, native(None, "client-tools"));
    features.insert(Capability::Caching, native(None, "prompt-caching-1h"));
    // Every current Claude model accepts base64 image blocks (Anthropic vision docs).
    features.insert(Capability::ImageInput, native(None, "image-input"));
    if profile == AnthropicPlatformCapabilityProfile::HostedProgrammaticTools {
        features.insert(
            Capability::HostedProgrammaticToolCalling,
            native(None, "hosted-programmatic-tools"),
        );
    }
    if matches!(
        profile,
        AnthropicPlatformCapabilityProfile::TextClientToolsAndThinking
            | AnthropicPlatformCapabilityProfile::AdaptiveClientTools
    ) {
        features.insert(
            Capability::Reasoning,
            native(
                Some(u64::from(maximum_output_tokens)),
                if profile == AnthropicPlatformCapabilityProfile::AdaptiveClientTools {
                    "adaptive-thinking;signed-replay"
                } else {
                    "manual-thinking-signed-replay"
                },
            ),
        );
    }
    CapabilitySet::new(features, BTreeMap::new())
}

fn reviewed_admission(
    config: &AnthropicPlatformModelConfig,
) -> Result<AdmissionRecord, AnthropicError> {
    // A family-rule match alone never admits: the id must also be a usable
    // row of the bundled reviewed table, under that row's own output limit.
    let adaptive = (config.profile.as_str() == ANTHROPIC_API_ADAPTIVE_PROFILE_ID
        && config.capabilities == AnthropicPlatformCapabilityProfile::AdaptiveClientTools
        && config.dialect == AnthropicMessagesDialect::stable())
    .then(|| crate::AnthropicApiModel::bundled(config.remote_id.as_str()).ok())
    .flatten()
    .and_then(|model| {
        model
            .family()
            .filter(|family| *family == config.family.as_str())
            .map(|family| (family, model.max_tokens()))
    })
    .filter(|(_, max_tokens)| config.maximum_output_tokens <= *max_tokens);
    if let Some((family, maximum_output_tokens)) = adaptive {
        return Ok(AdmissionRecord {
            remote_id: "",
            family,
            profile: ANTHROPIC_API_ADAPTIVE_PROFILE_ID,
            capabilities: AnthropicPlatformCapabilityProfile::AdaptiveClientTools,
            maximum_output_tokens,
        });
    }
    REVIEWED_ADMISSIONS
        .iter()
        .copied()
        .find(|admission| {
            config.remote_id.as_str() == admission.remote_id
                && config.family.as_str() == admission.family
                && config.profile.as_str() == admission.profile
                && config.capabilities == admission.capabilities
                && config.maximum_output_tokens <= admission.maximum_output_tokens
                && config.dialect == AnthropicMessagesDialect::stable()
        })
        .ok_or_else(|| {
            AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Anthropic model/family/profile/capability selection is not admitted by the current reviewed manifest",
            )
        })
}

fn admission_timestamp(seconds: u64) -> Result<Timestamp, AnthropicError> {
    Timestamp::new(seconds, 0).map_err(configuration_identity)
}

fn configuration_identity(error: impl std::fmt::Display) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        error.to_string(),
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
