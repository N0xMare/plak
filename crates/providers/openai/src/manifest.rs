//! Immutable reviewed production-model admission for the `OpenAI` Platform route.

use std::collections::BTreeMap;

use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, FactProvenance, FactSource, FeatureSupport,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ResolvedModelDescriptor, Support, Timestamp,
};
use plakous_model::{HostedTool, ModelRequest, ReasoningEffort, ReasoningPolicy, ToolChoice};

use crate::{ConfigError, OpenAiRoute, ResponsesFeature, StorageMode};

/// Revision of the exact built-in `OpenAI` Platform model manifest.
pub const OPENAI_PLATFORM_MODEL_MANIFEST_REVISION: &str = "openai-platform-models-2026-09-22-r3";
/// The only harness profile admitted by this manifest revision.
pub const OPENAI_GPT_56_PLATFORM_PROFILE_ID: &str = "openai.platform.gpt-5.6.2026-07-31.r2";
/// Reviewed GPT-6 Sol/Luna Responses profile.
pub const OPENAI_GPT_6_PLATFORM_PROFILE_ID: &str = "openai.platform.gpt-6.2026-09-22.r1";
/// UTC second at which the official model documentation was reviewed.
pub const OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS: u64 = 1_790_035_200;
/// Fixed end of the 90-day compatibility window for this review.
pub const OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS: u64 = 1_797_811_200;

const GPT_56_FAMILY_ID: &str = "gpt-5.6";
const GPT_6_FAMILY_ID: &str = "gpt-6";
/// Reviewed maximum output tokens for every Platform row.
pub(crate) const OPENAI_PLATFORM_MAX_OUTPUT_TOKENS: u32 = 128_000;

// Sources reviewed 2026-09-22:
// - https://developers.openai.com/api/docs/models
// - https://developers.openai.com/api/docs/guides/latest-model
// - the exact /api/docs/models/gpt-5.6-{sol,terra,luna} and
//   /api/docs/models/gpt-6-{sol,luna} pages
// - https://developers.openai.com/api/docs/guides/conversation-state
// - https://platform.openai.com/docs/models/default-usage-policies-by-endpoint
//
// Portable ReasoningEffort includes Ultra for subscription/Codex routes. The
// Platform rows admit only the current documented named GPT-5.6 efforts.
const REVIEWED_ROWS: [ReviewedPlatformRow; 5] = [
    ReviewedPlatformRow::new(
        "gpt-5.6-sol",
        GPT_56_FAMILY_ID,
        OPENAI_GPT_56_PLATFORM_PROFILE_ID,
    ),
    ReviewedPlatformRow::new(
        "gpt-5.6-terra",
        GPT_56_FAMILY_ID,
        OPENAI_GPT_56_PLATFORM_PROFILE_ID,
    ),
    ReviewedPlatformRow::new(
        "gpt-5.6-luna",
        GPT_56_FAMILY_ID,
        OPENAI_GPT_56_PLATFORM_PROFILE_ID,
    ),
    ReviewedPlatformRow::new(
        "gpt-6-sol",
        GPT_6_FAMILY_ID,
        OPENAI_GPT_6_PLATFORM_PROFILE_ID,
    ),
    ReviewedPlatformRow::new(
        "gpt-6-luna",
        GPT_6_FAMILY_ID,
        OPENAI_GPT_6_PLATFORM_PROFILE_ID,
    ),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReviewedPlatformRow {
    requested_model: &'static str,
    response_model: &'static str,
    family: &'static str,
    profile: &'static str,
    max_output_tokens: u32,
}

impl ReviewedPlatformRow {
    const fn new(model: &'static str, family: &'static str, profile: &'static str) -> Self {
        Self {
            requested_model: model,
            response_model: model,
            family,
            profile,
            max_output_tokens: OPENAI_PLATFORM_MAX_OUTPUT_TOKENS,
        }
    }
}

/// Crate-issued proof that one exact descriptor came from `REVIEWED_ROWS`.
///
/// The type is crate-visible only so the target constructor can retain it; its
/// field and the private marker cannot be constructed outside this module.
#[derive(Clone, Debug)]
pub(crate) struct ReviewedPlatformManifestWitness {
    row_index: usize,
    context: Option<std::sync::Arc<plakous_model::ContextContract>>,
    _private: PrivateManifestWitness,
}

#[derive(Clone, Debug)]
struct PrivateManifestWitness;

pub(crate) struct ReviewedPlatformAdmission {
    pub(crate) descriptor: ResolvedModelDescriptor,
    pub(crate) witness: ReviewedPlatformManifestWitness,
}

pub(crate) fn admit_reviewed_platform_model(
    route: &OpenAiRoute,
    requested_model: &ModelId,
    response_model: &ModelId,
    family: Option<&ModelFamilyId>,
    profile: &ModelAdapterProfileId,
    now: Timestamp,
) -> Result<ReviewedPlatformAdmission, ConfigError> {
    let row_index = REVIEWED_ROWS
        .iter()
        .position(|row| {
            requested_model.as_str() == row.requested_model
                && response_model.as_str() == row.response_model
                && family.map(ModelFamilyId::as_str) == Some(row.family)
                && profile.as_str() == row.profile
        })
        .ok_or(ConfigError::UnknownPlatformManifestTuple)?;
    ensure_manifest_current(now)?;
    let descriptor = descriptor_for_row(route, REVIEWED_ROWS[row_index])?;
    Ok(ReviewedPlatformAdmission {
        descriptor,
        witness: ReviewedPlatformManifestWitness {
            row_index,
            context: None,
            _private: PrivateManifestWitness,
        },
    })
}

pub(crate) fn validate_reviewed_platform_witness(
    route: &OpenAiRoute,
    descriptor: &ResolvedModelDescriptor,
    witness: &ReviewedPlatformManifestWitness,
    now: Timestamp,
) -> Result<(), ConfigError> {
    ensure_manifest_current(now)?;
    let row = REVIEWED_ROWS
        .get(witness.row_index)
        .copied()
        .ok_or(ConfigError::PlatformManifestBinding)?;
    let mut expected = descriptor_for_row(route, row)?;
    if let Some(context) = &witness.context {
        expected = expected.with_context_contract(context.as_ref().clone());
    }
    if &expected != descriptor {
        return Err(ConfigError::PlatformManifestBinding);
    }
    Ok(())
}

pub(crate) fn validate_reviewed_platform_request(
    descriptor: &ResolvedModelDescriptor,
    witness: &ReviewedPlatformManifestWitness,
    request: &ModelRequest,
) -> Result<(), ConfigError> {
    let row = REVIEWED_ROWS
        .get(witness.row_index)
        .copied()
        .ok_or(ConfigError::PlatformManifestBinding)?;
    if descriptor.requested_model().as_str() != row.requested_model
        || descriptor.reported_model().map(ModelId::as_str) != Some(row.response_model)
        || descriptor.model_family().map(ModelFamilyId::as_str) != Some(row.family)
        || descriptor.profile().as_str() != row.profile
    {
        return Err(ConfigError::PlatformManifestBinding);
    }
    if request
        .limits()
        .max_output_tokens()
        .is_some_and(|limit| limit.get() > row.max_output_tokens)
    {
        return Err(ConfigError::MaximumOutputTokensExceeded);
    }
    if !request.hosted_tools().is_empty()
        && (row.requested_model != "gpt-5.6-sol"
            || request.hosted_tools() != [HostedTool::ProgrammaticComposition])
    {
        return Err(ConfigError::UnsupportedToolForm);
    }
    validate_platform_adapter_request(request)
}

fn validate_platform_adapter_request(request: &ModelRequest) -> Result<(), ConfigError> {
    validate_adapter_request(request)?;
    if matches!(
        request.reasoning(),
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Ultra | ReasoningEffort::TokenBudget(_)),
            ..
        }
    ) {
        return Err(ConfigError::UnsupportedReasoningEffort);
    }
    Ok(())
}

#[cfg(feature = "native-auth")]
pub(crate) fn validate_unscoped_adapter_request(request: &ModelRequest) -> Result<(), ConfigError> {
    // A finite requested cap is a caller safety ceiling, not a claim about the
    // native catalog model's maximum. The request grammar already bounds it to
    // u32; a lower unsupported value can be rejected by the provider without
    // making the route unsafe or falsely promoting a capability fact.
    // Parallel concurrency is allowed here; encode still requires ParallelTools
    // capability evidence + route conformance.
    validate_adapter_request(request)
}

fn validate_adapter_request(request: &ModelRequest) -> Result<(), ConfigError> {
    if request.tools().iter().any(|tool| {
        plakous_responses_dialect::is_unprojected_string_custom_tool_definition(
            request,
            tool.definition(),
        )
    }) {
        return Err(ConfigError::UnsupportedToolForm);
    }
    match request.tool_choice() {
        ToolChoice::Auto | ToolChoice::None if request.tools().is_empty() => {}
        ToolChoice::Auto | ToolChoice::None | ToolChoice::Required
            if !request.tools().is_empty() => {}
        ToolChoice::Specific(name)
            if request
                .tools()
                .iter()
                .filter(|tool| tool.definition().name() == name)
                .count()
                == 1 => {}
        _ => return Err(ConfigError::InvalidToolChoice),
    }
    // Responses-family efforts for Codex subscription + the Platform API.
    // Low|Medium|High always; XHigh|Max|Ultra admitted for 5.6-class wire
    // (request.rs already maps them). Explicit `none` is the API ladder's
    // no-reasoning level. Codex catalog compile has no `none` bit, so a
    // subscription request does not reach here with it. TokenBudget is not
    // a Responses form.
    match request.reasoning() {
        ReasoningPolicy::Disabled
        | ReasoningPolicy::Enabled {
            effort:
                None
                | Some(
                    ReasoningEffort::None
                    | ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::XHigh
                    | ReasoningEffort::Max
                    | ReasoningEffort::Ultra,
                ),
            ..
        } => {}
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::TokenBudget(_)),
            ..
        } => return Err(ConfigError::UnsupportedReasoningBudget),
        _ => return Err(ConfigError::UnsupportedReasoningEffort),
    }
    Ok(())
}

fn descriptor_for_row(
    route: &OpenAiRoute,
    row: ReviewedPlatformRow,
) -> Result<ResolvedModelDescriptor, ConfigError> {
    let observed_at = fixed_timestamp(OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS)?;
    let expires_at = fixed_timestamp(OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS)?;
    let provenance = FactProvenance::new(FactSource::BuiltIn, observed_at, Some(expires_at));
    let mut features = BTreeMap::from([
        (
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(
                    None,
                    Some("Responses API streaming; exact manifest row".to_owned()),
                ),
                provenance,
            ),
        ),
        (
            Capability::Tools,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(
                    None,
                    Some("client function tools; non-parallel adapter subset".to_owned()),
                ),
                provenance,
            ),
        ),
        (
            Capability::Reasoning,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(
                    None,
                    Some("documented efforts low, medium, high, xhigh, max".to_owned()),
                ),
                provenance,
            ),
        ),
    ]);
    if route.storage() == StorageMode::ProviderStored
        && route
            .conformance()
            .supports(ResponsesFeature::PreviousResponseId)
    {
        features.insert(
            Capability::Continuation,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(
                    None,
                    Some(
                        "official Platform /v1/responses route with store=true; exact response-ID prefix reuse"
                            .to_owned(),
                    ),
                ),
                FactProvenance::new(FactSource::RouteEvidence, observed_at, Some(expires_at)),
            ),
        );
    }
    if row.requested_model == "gpt-5.6-sol"
        && route.access_mode() == crate::AccessMode::LocalFixture
    {
        features.insert(
            Capability::HostedProgrammaticToolCalling,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(
                    None,
                    Some("OpenAI Platform programmatic tool calling; exact gpt-5.6-sol offline profile".to_owned()),
                ),
                FactProvenance::new(FactSource::RouteEvidence, observed_at, Some(expires_at)),
            ),
        );
    }
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        route.dialect().protocol().clone(),
        route.id().clone(),
        ModelId::new(row.requested_model).map_err(|_| ConfigError::PlatformManifestBinding)?,
        Some(ModelId::new(row.response_model).map_err(|_| ConfigError::PlatformManifestBinding)?),
        Some(ModelFamilyId::new(row.family).map_err(|_| ConfigError::PlatformManifestBinding)?),
        ModelAdapterProfileId::new(row.profile)
            .map_err(|_| ConfigError::PlatformManifestBinding)?,
        CapabilitySet::new(features, BTreeMap::new()),
    ))
}

fn ensure_manifest_current(now: Timestamp) -> Result<(), ConfigError> {
    let reviewed_at = fixed_timestamp(OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS)?;
    let expires_at = fixed_timestamp(OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS)?;
    if now < reviewed_at || expires_at <= now {
        return Err(ConfigError::PlatformManifestEvidenceExpired);
    }
    Ok(())
}

fn fixed_timestamp(seconds: u64) -> Result<Timestamp, ConfigError> {
    Timestamp::new(seconds, 0).map_err(|_| ConfigError::PlatformManifestBinding)
}

/// Attaches an explicit caller operating envelope without claiming provider capacity.
pub(crate) fn with_context_envelope(
    mut admission: ReviewedPlatformAdmission,
    route: &OpenAiRoute,
    maximum: std::num::NonZeroU64,
) -> Result<ReviewedPlatformAdmission, ConfigError> {
    use plakous_model::{
        CapacityCertificate, CapacityRule, ContextContract, ContextEvidenceSource,
        CountCertificateKind, Digest, DigestDomain, EvidenceFreshness, MeasurementCertificate,
        ProtocolId, RuntimeModelAuthority,
    };
    let build = || -> Result<ContextContract, Box<dyn std::error::Error>> {
        let binding = RuntimeModelAuthority::authority_class_for(
            route.account_binding()?,
            route.credential_authority_binding()?,
        )?;
        let declaration = Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[
                b"openai-platform-caller-wire-envelope-v1",
                &maximum.get().to_be_bytes(),
            ],
        )?;
        let freshness = EvidenceFreshness::new(
            fixed_timestamp(OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS)?,
            Some(fixed_timestamp(
                OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS,
            )?),
        )?;
        let capacity = CapacityCertificate::custom_envelope(
            "openai-platform-caller-input-envelope",
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
            "openai-platform-complete-wire-bytes",
            "v1",
            "complete-lowered-wire-byte-upper-bound",
            "v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(binding),
            freshness,
        )?;
        Ok(ContextContract::new(
            ProtocolId::new("responses-family")?,
            "responses-wire-bytes-openai-platform-v1",
            "responses-canonical-openai-platform-v1",
            "openai-platform-output-reservation-v1",
            capacity,
            vec![measurement],
        )?)
    };
    let context = build().map_err(|_| ConfigError::LocalFixtureContext)?;
    admission.descriptor = admission.descriptor.with_context_contract(context.clone());
    admission.witness.context = Some(std::sync::Arc::new(context));
    Ok(admission)
}
