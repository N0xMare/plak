//! Data-only harness profiles and deterministic plan compilation.
//!
//! This module implements the model-facing half of the three-domain ownership
//! boundary:
//!
//! - [`HarnessProfileSpec`] is immutable policy data.
//! - [`HarnessProfileCompiler`] is pure and deterministic.
//! - [`ResolvedHarnessPlan`] is an immutable, content-addressed plan.
//!
//! Profiles cannot select routes or credentials, mint capabilities, authorize
//! effects, mutate history, or inject executable callbacks into the agent loop.

mod compiler;
mod specification;
pub(crate) use compiler::compile_construction_bound_plan;
pub use compiler::compile_explicit_profile;
pub use compiler::compiler_model_facts_from_model;
mod errors;
mod plan;
pub use errors::ProfileError;

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use plakous_model::{
    CachePolicy, Capability, CapabilityRequirement, CompiledProviderConfiguration, Digest,
    DigestDomain, DigestError, EffectiveProviderConfigurationId, InstructionBlock,
    InstructionSource, LanguageModel, ModelAdapterProfileId, ModelId, ProtocolId, ReasoningEffort,
    ReasoningEffortDisposition, ReasoningPolicy, ResolvedModelDescriptor, RouteId, SafeDiagnostic,
    SelectedProviderConfigurationId, Support,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version of the portable harness profile specification.
pub(crate) const HARNESS_PROFILE_SCHEMA_VERSION: u32 = 2;
/// Compiler revision bound into every resolved plan.
pub(crate) const HARNESS_PROFILE_COMPILER_REVISION: u32 = 5;
/// Closed strategy catalog revision bound into every resolved plan.
pub(crate) const HARNESS_STRATEGY_CATALOG_REVISION: u32 = 3;
/// Canonical persisted encoding revision for [`HarnessProfileSpec`].
pub(crate) const HARNESS_PROFILE_ENCODING_VERSION: u32 = 2;
/// Canonical persisted encoding revision for [`HarnessCompilerInputs`].
pub(crate) const HARNESS_COMPILER_INPUT_ENCODING_VERSION: u32 = 4;
/// Canonical persisted encoding revision for [`ResolvedHarnessPlan`].
pub(crate) const RESOLVED_HARNESS_PLAN_ENCODING_VERSION: u32 = 4;

const SPEC_DIGEST_VERSION: u32 = 1;
const PLAN_DIGEST_VERSION: u32 = 2;
const CATALOG_DIGEST_VERSION: u32 = 1;
const CONTEXT_SOURCE_DIGEST_VERSION: u32 = 1;
/// Maximum base/composed instruction budget for harness profiles.
pub const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;
const MAX_TEMPLATE_BYTES: usize = 8 * 1024;
const MAX_SOURCE_SELECTORS: usize = 32;
const MAX_TOOL_EXPOSURE_OVERRIDES: usize = 64;
const MAX_DIAGNOSTICS: usize = 64;
const MAX_CONTEXT_SOURCE_METADATA_BYTES: usize = 128;

/// Stable identity of one model-facing harness profile.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessProfileId(String);

/// Semantic version of a harness profile specification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ProfileSemanticVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

/// Provenance of a portable harness profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum HarnessProfileProvenance {
    /// Built-in recommended profile data shipped with Plakous.
    Recommended,
    /// Operator-supplied profile data accepted through an explicit construction path.
    Operator,
    /// Synthetic profile used only by deterministic tests.
    Synthetic,
}

/// Closed instruction-composition strategies owned by the neutral core.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum InstructionCompositionStrategy {
    /// Concatenate base text then ordered registered source content.
    BaseThenSources,
    /// Use only the explicit base instruction text.
    BaseOnly,
}

/// Closed tool-exposure strategies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolExposureStrategy {
    /// Expose every tool in the frozen registry under its canonical name.
    CanonicalAll,
    /// Expose none of the tools.
    None,
}

/// Closed reasoning preference strategies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ReasoningPreferenceStrategy {
    /// Leave reasoning disabled.
    Disabled,
    /// Prefer the named effort when the model supports reasoning.
    PreferEffort,
}

/// Closed continuation preference strategies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContinuationPreferenceStrategy {
    /// Prefer native continuation when the protocol route supports it.
    PreferNative,
    /// Always fall back to full canonical history replay.
    CanonicalReplayOnly,
}

/// Closed prompt-cache preference strategies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CachePreferenceStrategy {
    /// Prefer stable-prefix cache identity when supported.
    PreferStablePrefix,
    /// Do not request provider prompt caching.
    Disabled,
}

/// One registered instruction/context source selector.
///
/// Selectors name sources; they never embed ambient paths, handles, or
/// credential status. Resolution happens outside the pure compiler.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSourceSelector {
    source_id: String,
}

/// One model-facing tool name/description override over a canonical tool.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExposureOverride {
    canonical_name: String,
    exposed_name: String,
    description: Option<String>,
}

/// Immutable data-only harness profile specification.
///
/// Unknown JSON fields are rejected. The type contains no callbacks, secrets,
/// routes, credentials, or capability assertions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProfileSpec {
    id: HarnessProfileId,
    schema_version: u32,
    semantic_version: ProfileSemanticVersion,
    provenance: HarnessProfileProvenance,
    display_name: Option<String>,
    base_instructions: String,
    instruction_strategy: InstructionCompositionStrategy,
    instruction_sources: Vec<ContextSourceSelector>,
    tool_exposure_strategy: ToolExposureStrategy,
    tool_overrides: Vec<ToolExposureOverride>,
    reasoning_strategy: ReasoningPreferenceStrategy,
    preferred_reasoning_effort: Option<ReasoningEffort>,
    continuation_strategy: ContinuationPreferenceStrategy,
    cache_strategy: CachePreferenceStrategy,
}

/// Builder for [`HarnessProfileSpec`].
#[derive(Clone, Debug)]
pub struct HarnessProfileSpecBuilder {
    id: Option<HarnessProfileId>,
    semantic_version: ProfileSemanticVersion,
    provenance: HarnessProfileProvenance,
    display_name: Option<String>,
    base_instructions: String,
    instruction_strategy: InstructionCompositionStrategy,
    instruction_sources: Vec<ContextSourceSelector>,
    tool_exposure_strategy: ToolExposureStrategy,
    tool_overrides: Vec<ToolExposureOverride>,
    reasoning_strategy: ReasoningPreferenceStrategy,
    preferred_reasoning_effort: Option<ReasoningEffort>,
    continuation_strategy: ContinuationPreferenceStrategy,
    cache_strategy: CachePreferenceStrategy,
}

/// Content-addressed identity of an immutable context-source object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedContextSourceRef {
    reference_id: String,
    content_digest: Digest,
    version: String,
}

/// Immutable, reconstruction-complete facts for one resolved context source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedContextSourceFact {
    source_id: String,
    content: Arc<str>,
    content_digest: Digest,
    version: String,
    provenance: String,
    immutable_reference: Option<VerifiedContextSourceRef>,
}

/// Support fact used only as a constraint on preferred profile behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum FeatureSupportFact {
    /// The feature is supported by the already-selected model/route.
    Supported,
    /// Support is unknown; required requests fail and preferences downgrade.
    Unknown,
    /// The feature is unsupported.
    Unsupported,
}

/// Already-selected model/route/capability facts bound into compilation.
///
/// These facts are inputs. The compiler cannot invent or widen them, and the
/// resulting plan cannot replace the selected route or model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerModelFacts {
    selected_provider_configuration_id: SelectedProviderConfigurationId,
    effective_provider_configuration_id: EffectiveProviderConfigurationId,
    protocol: ProtocolId,
    route: RouteId,
    model: ModelId,
    model_adapter_profile: ModelAdapterProfileId,
    model_binding_digest: Digest,
    reasoning_support: FeatureSupportFact,
    native_continuation_support: FeatureSupportFact,
    prompt_cache_support: FeatureSupportFact,
}

/// Frozen tool-registry facts available to compilation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerToolFacts {
    registry_digest: Digest,
    canonical_tool_names: Vec<String>,
}

/// Execution-environment and runtime-policy facts available to compilation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerRuntimeFacts {
    execution_environment_digest: Digest,
    max_instruction_bytes: usize,
    require_reasoning: bool,
    require_native_continuation: bool,
    require_prompt_cache: bool,
}

/// Complete pure inputs for harness-profile compilation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCompilerInputs {
    selected_provider_configuration_id: SelectedProviderConfigurationId,
    effective_provider_configuration_id: EffectiveProviderConfigurationId,
    model: CompilerModelFacts,
    tools: CompilerToolFacts,
    runtime: CompilerRuntimeFacts,
    context_sources: Vec<ResolvedContextSourceFact>,
}

/// Resolved instruction plan selected by the compiler.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedInstructionPlan {
    strategy: InstructionCompositionStrategy,
    composed_instructions: String,
    sources: Vec<ResolvedContextSourceFact>,
}

/// Resolved tool-exposure decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedToolExposurePlan {
    strategy: ToolExposureStrategy,
    exposed_names: Vec<String>,
    overrides: Vec<ToolExposureOverride>,
}

/// Resolved reasoning decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedReasoningPlan {
    enabled: bool,
    effort: Option<ReasoningEffort>,
}

/// Resolved continuation decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedContinuationPlan {
    use_native_continuation: bool,
}

/// Resolved cache decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedCachePlan {
    prefer_stable_prefix: bool,
}

/// Immutable, content-addressed compiled harness plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedHarnessPlan {
    inner: Arc<ResolvedHarnessPlanInner>,
    source: Arc<ResolvedHarnessPlanSource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedHarnessPlanSource {
    profile: HarnessProfileSpec,
    inputs: HarnessCompilerInputs,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ResolvedHarnessPlanInner {
    profile_id: HarnessProfileId,
    profile_digest: Digest,
    plan_digest: Digest,
    catalog_digest: Digest,
    compiler_revision: u32,
    strategy_catalog_revision: u32,
    schema_version: u32,
    semantic_version: ProfileSemanticVersion,
    provenance: HarnessProfileProvenance,
    selected_provider_configuration_id: SelectedProviderConfigurationId,
    effective_provider_configuration_id: EffectiveProviderConfigurationId,
    model: CompilerModelFacts,
    tool_registry_digest: Digest,
    execution_environment_digest: Digest,
    instructions: ResolvedInstructionPlan,
    tools: ResolvedToolExposurePlan,
    reasoning: ResolvedReasoningPlan,
    continuation: ResolvedContinuationPlan,
    cache: ResolvedCachePlan,
    diagnostics: Vec<SafeDiagnostic>,
}

/// Pure deterministic harness-profile compiler.
#[derive(Clone, Copy, Debug, Default)]
pub struct HarnessProfileCompiler;

fn context_source_digest(content: &str) -> Result<Digest, ProfileError> {
    Digest::sha256(
        DigestDomain::Blob,
        CONTEXT_SOURCE_DIGEST_VERSION,
        &[content.as_bytes()],
    )
    .map_err(ProfileError::from)
}

fn validate_source_digest(digest: Digest) -> Result<(), ProfileError> {
    if digest.domain() != DigestDomain::Blob || digest.version() != CONTEXT_SOURCE_DIGEST_VERSION {
        return Err(ProfileError::InvalidSourceDigestDomain);
    }
    Ok(())
}

fn validate_source_metadata(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_CONTEXT_SOURCE_METADATA_BYTES
}

fn validate_context_sources(sources: &[ResolvedContextSourceFact]) -> Result<(), ProfileError> {
    if sources.len() > MAX_SOURCE_SELECTORS {
        return Err(ProfileError::TooManySources);
    }
    let mut identities = BTreeSet::new();
    let mut total_content_bytes = 0_usize;
    for source in sources {
        source.validate()?;
        total_content_bytes = total_content_bytes
            .checked_add(source.content().len())
            .ok_or(ProfileError::InstructionBudgetExceeded {
                max_bytes: MAX_INSTRUCTION_BYTES,
            })?;
        if total_content_bytes > MAX_INSTRUCTION_BYTES {
            return Err(ProfileError::InstructionBudgetExceeded {
                max_bytes: MAX_INSTRUCTION_BYTES,
            });
        }
        if !identities.insert(source.source_id()) {
            return Err(ProfileError::DuplicateContextSource {
                source_id: source.source_id().to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    value.bytes().all(|byte| {
        matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'
        )
    })
}

fn validate_text_bound(value: &str, max_bytes: usize) -> Result<(), ProfileError> {
    if value.len() > max_bytes {
        return Err(ProfileError::TextTooLarge {
            max_bytes,
            found: value.len(),
        });
    }
    Ok(())
}

fn validate_decoded_spec(spec: &HarnessProfileSpec) -> Result<(), ProfileError> {
    if spec.schema_version != HARNESS_PROFILE_SCHEMA_VERSION {
        return Err(ProfileError::UnsupportedSchemaVersion {
            found: spec.schema_version,
        });
    }
    if !validate_identifier(spec.id.as_str()) {
        return Err(ProfileError::InvalidIdentity);
    }
    validate_text_bound(&spec.base_instructions, MAX_INSTRUCTION_BYTES)?;
    if spec.instruction_sources.len() > MAX_SOURCE_SELECTORS {
        return Err(ProfileError::TooManySources);
    }
    for source in &spec.instruction_sources {
        if !validate_identifier(source.source_id()) {
            return Err(ProfileError::InvalidSourceSelector);
        }
    }
    if spec.tool_overrides.len() > MAX_TOOL_EXPOSURE_OVERRIDES {
        return Err(ProfileError::TooManyToolOverrides);
    }
    for entry in &spec.tool_overrides {
        if !validate_identifier(entry.canonical_name())
            || !validate_identifier(entry.exposed_name())
        {
            return Err(ProfileError::InvalidToolOverride);
        }
        if let Some(description) = entry.description() {
            if description.is_empty() {
                return Err(ProfileError::InvalidToolOverride);
            }
            validate_text_bound(description, MAX_TEMPLATE_BYTES)?;
        }
    }
    if let Some(display_name) = spec.display_name.as_deref() {
        validate_text_bound(display_name, MAX_TEMPLATE_BYTES)?;
    }
    match (spec.reasoning_strategy, spec.preferred_reasoning_effort) {
        (ReasoningPreferenceStrategy::PreferEffort, None) => {
            Err(ProfileError::MissingReasoningEffort)
        }
        (ReasoningPreferenceStrategy::Disabled, Some(_)) => {
            Err(ProfileError::UnexpectedReasoningEffort)
        }
        _ => Ok(()),
    }
}

pub(crate) fn validate_decoded_compiler_inputs(
    inputs: &HarnessCompilerInputs,
) -> Result<(), ProfileError> {
    if inputs.selected_provider_configuration_id != inputs.model.selected_provider_configuration_id
        || inputs.effective_provider_configuration_id
            != inputs.model.effective_provider_configuration_id
        || inputs.model.model_binding_digest.domain() != DigestDomain::ModelBinding
        || inputs.tools.registry_digest.domain() != DigestDomain::ToolDefinition
        || inputs.runtime.execution_environment_digest.domain()
            != DigestDomain::ExecutionEnvironment
    {
        return Err(ProfileError::InvalidCompilerDigestDomain);
    }
    validate_context_sources(&inputs.context_sources)?;
    CompilerToolFacts::new(
        inputs.tools.registry_digest,
        inputs.tools.canonical_tool_names.clone(),
    )?;
    if inputs.runtime.max_instruction_bytes == 0
        || inputs.runtime.max_instruction_bytes > MAX_INSTRUCTION_BYTES
    {
        return Err(ProfileError::InstructionBudgetExceeded {
            max_bytes: MAX_INSTRUCTION_BYTES,
        });
    }
    Ok(())
}

pub(crate) fn canonical_profile_bytes(
    profile: &HarnessProfileSpec,
) -> Result<Vec<u8>, ProfileError> {
    serde_json::to_vec(profile).map_err(|error| ProfileError::Serialization {
        reason: error.to_string(),
    })
}

pub(crate) fn decode_canonical_profile(bytes: &[u8]) -> Result<HarnessProfileSpec, ProfileError> {
    let profile = ProfileError::decode_spec(bytes)?;
    if canonical_profile_bytes(&profile)? != bytes {
        return Err(ProfileError::InvalidJson {
            reason: "non-canonical profile encoding".to_owned(),
        });
    }
    Ok(profile)
}

pub(crate) fn canonical_compiler_input_bytes(
    inputs: &HarnessCompilerInputs,
) -> Result<Vec<u8>, ProfileError> {
    serde_json::to_vec(inputs).map_err(|error| ProfileError::Serialization {
        reason: error.to_string(),
    })
}

pub(crate) fn decode_canonical_compiler_inputs(
    bytes: &[u8],
) -> Result<HarnessCompilerInputs, ProfileError> {
    let inputs: HarnessCompilerInputs =
        serde_json::from_slice(bytes).map_err(|error| ProfileError::InvalidJson {
            reason: error.to_string(),
        })?;
    validate_decoded_compiler_inputs(&inputs)?;
    if canonical_compiler_input_bytes(&inputs)? != bytes {
        return Err(ProfileError::InvalidJson {
            reason: "non-canonical compiler-input encoding".to_owned(),
        });
    }
    Ok(inputs)
}

pub(crate) fn canonical_plan_bytes(plan: &ResolvedHarnessPlan) -> Result<Vec<u8>, ProfileError> {
    serde_json::to_vec(&*plan.inner).map_err(|error| ProfileError::Serialization {
        reason: error.to_string(),
    })
}

/// Recommended neutral coding profile used as a portable starting point.
///
/// Recommended data lives outside provider protocol crates so model-facing
/// behavior is not confused with wire or subscription integration truth.
///
/// # Errors
///
/// Returns a profile error when the recommended constants fail validation.
pub fn recommended_neutral_coding_profile() -> Result<HarnessProfileSpec, ProfileError> {
    HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plakous.neutral.coding.v1")?)
        .semantic_version(ProfileSemanticVersion::new(0, 2, 0))
        .provenance(HarnessProfileProvenance::Recommended)
        .display_name("Neutral coding")
        .base_instructions(
            "You are a careful coding agent. Prefer exact tools, preserve secrets, and explain only what is necessary.",
        )
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
        .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
        .preferred_reasoning_effort(Some(ReasoningEffort::High))
        .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
