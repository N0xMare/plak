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
pub const HARNESS_PROFILE_SCHEMA_VERSION: u32 = 2;
/// Compiler revision bound into every resolved plan.
pub const HARNESS_PROFILE_COMPILER_REVISION: u32 = 5;
/// Closed strategy catalog revision bound into every resolved plan.
pub const HARNESS_STRATEGY_CATALOG_REVISION: u32 = 3;
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

impl HarnessProfileId {
    /// Validates a portable harness-profile identity.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::InvalidIdentity`] when the value is empty,
    /// oversized, or outside the portable identifier alphabet.
    pub fn new(value: impl Into<String>) -> Result<Self, ProfileError> {
        let value = value.into();
        if !validate_identifier(&value) {
            return Err(ProfileError::InvalidIdentity);
        }
        Ok(Self(value))
    }

    /// Returns the identity text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HarnessProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Semantic version of a harness profile specification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ProfileSemanticVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

impl ProfileSemanticVersion {
    /// Creates a semantic version triple.
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Returns the major component.
    #[must_use]
    pub const fn major(self) -> u16 {
        self.major
    }

    /// Returns the minor component.
    #[must_use]
    pub const fn minor(self) -> u16 {
        self.minor
    }

    /// Returns the patch component.
    #[must_use]
    pub const fn patch(self) -> u16 {
        self.patch
    }
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

impl ContextSourceSelector {
    /// Creates a bounded registered-source selector.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::InvalidSourceSelector`] for empty, oversized, or
    /// non-portable identifiers.
    pub fn new(source_id: impl Into<String>) -> Result<Self, ProfileError> {
        let source_id = source_id.into();
        if !validate_identifier(&source_id) {
            return Err(ProfileError::InvalidSourceSelector);
        }
        Ok(Self { source_id })
    }

    /// Returns the registered source identity.
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
}

/// One model-facing tool name/description override over a canonical tool.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExposureOverride {
    canonical_name: String,
    exposed_name: String,
    description: Option<String>,
}

impl ToolExposureOverride {
    /// Creates a single exposure override.
    ///
    /// # Errors
    ///
    /// Returns a profile error for empty or oversized names/descriptions.
    pub fn new(
        canonical_name: impl Into<String>,
        exposed_name: impl Into<String>,
        description: Option<String>,
    ) -> Result<Self, ProfileError> {
        let canonical_name = canonical_name.into();
        let exposed_name = exposed_name.into();
        if !validate_identifier(&canonical_name) {
            return Err(ProfileError::InvalidToolOverride);
        }
        if !validate_identifier(&exposed_name) {
            return Err(ProfileError::InvalidToolOverride);
        }
        if let Some(description) = &description
            && (description.is_empty() || description.len() > MAX_TEMPLATE_BYTES)
        {
            return Err(ProfileError::InvalidToolOverride);
        }
        Ok(Self {
            canonical_name,
            exposed_name,
            description,
        })
    }

    /// Returns the canonical tool identity being presented.
    #[must_use]
    pub fn canonical_name(&self) -> &str {
        &self.canonical_name
    }

    /// Returns the model-facing exposed name.
    #[must_use]
    pub fn exposed_name(&self) -> &str {
        &self.exposed_name
    }

    /// Returns the optional model-facing description override.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
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

impl HarnessProfileSpec {
    /// Builds a validated specification from a builder.
    ///
    /// # Errors
    ///
    /// Returns a profile error when any field violates bounds or closed-catalog
    /// constraints.
    pub fn try_new(builder: HarnessProfileSpecBuilder) -> Result<Self, ProfileError> {
        builder.build()
    }

    /// Returns the stable profile identity.
    #[must_use]
    pub const fn id(&self) -> &HarnessProfileId {
        &self.id
    }

    /// Returns the schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Returns the semantic version.
    #[must_use]
    pub const fn semantic_version(&self) -> ProfileSemanticVersion {
        self.semantic_version
    }

    /// Returns the provenance class.
    #[must_use]
    pub const fn provenance(&self) -> HarnessProfileProvenance {
        self.provenance
    }

    /// Returns the optional display name.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Returns base instruction text.
    #[must_use]
    pub fn base_instructions(&self) -> &str {
        &self.base_instructions
    }

    /// Returns the instruction composition strategy.
    #[must_use]
    pub const fn instruction_strategy(&self) -> InstructionCompositionStrategy {
        self.instruction_strategy
    }

    /// Returns registered instruction/context source selectors.
    #[must_use]
    pub fn instruction_sources(&self) -> &[ContextSourceSelector] {
        &self.instruction_sources
    }

    /// Returns the tool exposure strategy.
    #[must_use]
    pub const fn tool_exposure_strategy(&self) -> ToolExposureStrategy {
        self.tool_exposure_strategy
    }

    /// Returns model-facing tool overrides.
    #[must_use]
    pub fn tool_overrides(&self) -> &[ToolExposureOverride] {
        &self.tool_overrides
    }

    /// Returns the reasoning preference strategy.
    #[must_use]
    pub const fn reasoning_strategy(&self) -> ReasoningPreferenceStrategy {
        self.reasoning_strategy
    }

    /// Returns the preferred reasoning effort, when applicable.
    #[must_use]
    pub const fn preferred_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.preferred_reasoning_effort
    }

    /// Returns a copy of this profile with operator-selected reasoning effort.
    ///
    /// Forces [`ReasoningPreferenceStrategy::PreferEffort`] so explicit harness
    /// plans honor CLI / application effort flags (plan reasoning overwrites
    /// [`crate::AgentRequestPolicy`] at agent construction).
    ///
    /// # Errors
    ///
    /// Returns a profile error when rebuilt fields violate profile bounds.
    pub fn with_preferred_reasoning_effort(
        &self,
        effort: ReasoningEffort,
    ) -> Result<Self, ProfileError> {
        let mut builder = HarnessProfileSpecBuilder::new()
            .id(self.id.clone())
            .semantic_version(self.semantic_version)
            .provenance(self.provenance)
            .base_instructions(self.base_instructions.clone())
            .instruction_strategy(self.instruction_strategy)
            .instruction_sources(self.instruction_sources.clone())
            .tool_exposure_strategy(self.tool_exposure_strategy)
            .tool_overrides(self.tool_overrides.clone())
            .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
            .preferred_reasoning_effort(Some(effort))
            .continuation_strategy(self.continuation_strategy)
            .cache_strategy(self.cache_strategy);
        if let Some(name) = &self.display_name {
            builder = builder.display_name(name.clone());
        }
        builder.build()
    }

    /// Returns the continuation preference strategy.
    #[must_use]
    pub const fn continuation_strategy(&self) -> ContinuationPreferenceStrategy {
        self.continuation_strategy
    }

    /// Returns the cache preference strategy.
    #[must_use]
    pub const fn cache_strategy(&self) -> CachePreferenceStrategy {
        self.cache_strategy
    }

    /// Computes the content digest of this specification.
    ///
    /// # Errors
    ///
    /// Returns a digest or serialization error.
    pub fn content_digest(&self) -> Result<Digest, ProfileError> {
        let bytes = serde_json::to_vec(self).map_err(|error| ProfileError::Serialization {
            reason: error.to_string(),
        })?;
        Digest::sha256(
            DigestDomain::HarnessProfileSpec,
            SPEC_DIGEST_VERSION,
            &[&bytes],
        )
        .map_err(ProfileError::from)
    }
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

impl Default for HarnessProfileSpecBuilder {
    fn default() -> Self {
        Self {
            id: None,
            semantic_version: ProfileSemanticVersion::new(0, 1, 0),
            provenance: HarnessProfileProvenance::Synthetic,
            display_name: None,
            base_instructions: String::new(),
            instruction_strategy: InstructionCompositionStrategy::BaseOnly,
            instruction_sources: Vec::new(),
            tool_exposure_strategy: ToolExposureStrategy::CanonicalAll,
            tool_overrides: Vec::new(),
            reasoning_strategy: ReasoningPreferenceStrategy::Disabled,
            preferred_reasoning_effort: None,
            continuation_strategy: ContinuationPreferenceStrategy::CanonicalReplayOnly,
            cache_strategy: CachePreferenceStrategy::Disabled,
        }
    }
}

impl HarnessProfileSpecBuilder {
    /// Creates an empty builder with conservative defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the stable profile identity.
    #[must_use]
    pub fn id(mut self, id: HarnessProfileId) -> Self {
        self.id = Some(id);
        self
    }

    /// Sets the semantic version.
    #[must_use]
    pub const fn semantic_version(mut self, version: ProfileSemanticVersion) -> Self {
        self.semantic_version = version;
        self
    }

    /// Sets provenance.
    #[must_use]
    pub const fn provenance(mut self, provenance: HarnessProfileProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Sets an optional display name (not identity).
    #[must_use]
    pub fn display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Sets base instructions.
    #[must_use]
    pub fn base_instructions(mut self, text: impl Into<String>) -> Self {
        self.base_instructions = text.into();
        self
    }

    /// Sets the instruction composition strategy.
    #[must_use]
    pub const fn instruction_strategy(mut self, strategy: InstructionCompositionStrategy) -> Self {
        self.instruction_strategy = strategy;
        self
    }

    /// Sets registered instruction sources.
    #[must_use]
    pub fn instruction_sources(mut self, sources: Vec<ContextSourceSelector>) -> Self {
        self.instruction_sources = sources;
        self
    }

    /// Sets the tool exposure strategy.
    #[must_use]
    pub const fn tool_exposure_strategy(mut self, strategy: ToolExposureStrategy) -> Self {
        self.tool_exposure_strategy = strategy;
        self
    }

    /// Sets tool presentation overrides.
    #[must_use]
    pub fn tool_overrides(mut self, overrides: Vec<ToolExposureOverride>) -> Self {
        self.tool_overrides = overrides;
        self
    }

    /// Sets the reasoning preference strategy.
    #[must_use]
    pub const fn reasoning_strategy(mut self, strategy: ReasoningPreferenceStrategy) -> Self {
        self.reasoning_strategy = strategy;
        self
    }

    /// Sets the preferred reasoning effort.
    #[must_use]
    pub const fn preferred_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.preferred_reasoning_effort = effort;
        self
    }

    /// Sets the continuation preference strategy.
    #[must_use]
    pub const fn continuation_strategy(mut self, strategy: ContinuationPreferenceStrategy) -> Self {
        self.continuation_strategy = strategy;
        self
    }

    /// Sets the cache preference strategy.
    #[must_use]
    pub const fn cache_strategy(mut self, strategy: CachePreferenceStrategy) -> Self {
        self.cache_strategy = strategy;
        self
    }

    /// Validates and freezes the specification.
    ///
    /// # Errors
    ///
    /// Returns a profile error for missing identity or bound violations.
    pub fn build(self) -> Result<HarnessProfileSpec, ProfileError> {
        let id = self.id.ok_or(ProfileError::MissingIdentity)?;
        validate_text_bound(&self.base_instructions, MAX_INSTRUCTION_BYTES)?;
        if self.instruction_sources.len() > MAX_SOURCE_SELECTORS {
            return Err(ProfileError::TooManySources);
        }
        if self.tool_overrides.len() > MAX_TOOL_EXPOSURE_OVERRIDES {
            return Err(ProfileError::TooManyToolOverrides);
        }
        if let Some(name) = &self.display_name {
            validate_text_bound(name, MAX_TEMPLATE_BYTES)?;
        }
        if self.reasoning_strategy == ReasoningPreferenceStrategy::PreferEffort
            && self.preferred_reasoning_effort.is_none()
        {
            return Err(ProfileError::MissingReasoningEffort);
        }
        if self.reasoning_strategy == ReasoningPreferenceStrategy::Disabled
            && self.preferred_reasoning_effort.is_some()
        {
            return Err(ProfileError::UnexpectedReasoningEffort);
        }
        Ok(HarnessProfileSpec {
            id,
            schema_version: HARNESS_PROFILE_SCHEMA_VERSION,
            semantic_version: self.semantic_version,
            provenance: self.provenance,
            display_name: self.display_name,
            base_instructions: self.base_instructions,
            instruction_strategy: self.instruction_strategy,
            instruction_sources: self.instruction_sources,
            tool_exposure_strategy: self.tool_exposure_strategy,
            tool_overrides: self.tool_overrides,
            reasoning_strategy: self.reasoning_strategy,
            preferred_reasoning_effort: self.preferred_reasoning_effort,
            continuation_strategy: self.continuation_strategy,
            cache_strategy: self.cache_strategy,
        })
    }
}

/// Content-addressed identity of an immutable context-source object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedContextSourceRef {
    reference_id: String,
    content_digest: Digest,
    version: String,
}

impl VerifiedContextSourceRef {
    /// Creates an immutable reference whose content and version must be verified
    /// before the reference enters compiler state.
    ///
    /// # Errors
    ///
    /// Returns a profile error for an invalid reference identity, source
    /// version, digest domain, or digest format version.
    pub fn new(
        reference_id: impl Into<String>,
        content_digest: Digest,
        version: impl Into<String>,
    ) -> Result<Self, ProfileError> {
        let reference_id = reference_id.into();
        let version = version.into();
        let reference = Self {
            reference_id,
            content_digest,
            version,
        };
        reference.validate()?;
        Ok(reference)
    }

    /// Returns the portable immutable-object identity.
    #[must_use]
    pub fn reference_id(&self) -> &str {
        &self.reference_id
    }

    /// Returns the expected exact-content digest.
    #[must_use]
    pub const fn content_digest(&self) -> &Digest {
        &self.content_digest
    }

    /// Returns the expected source version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    fn validate(&self) -> Result<(), ProfileError> {
        if !validate_identifier(&self.reference_id) || !validate_source_metadata(&self.version) {
            return Err(ProfileError::InvalidContextSourceReference);
        }
        validate_source_digest(self.content_digest)
    }
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

impl ResolvedContextSourceFact {
    /// Creates a resolved source fact containing exact model-visible content.
    ///
    /// # Errors
    ///
    /// Returns a profile error for empty or non-portable fields.
    pub fn new(
        source_id: impl Into<String>,
        content: impl Into<String>,
        version: impl Into<String>,
        provenance: impl Into<String>,
    ) -> Result<Self, ProfileError> {
        let source_id = source_id.into();
        let content = content.into();
        let version = version.into();
        let provenance = provenance.into();
        let content_digest = context_source_digest(&content)?;
        let fact = Self {
            source_id,
            content: Arc::from(content),
            content_digest,
            version,
            provenance,
            immutable_reference: None,
        };
        fact.validate()?;
        Ok(fact)
    }

    /// Creates a fact after verifying exact content and observed version against
    /// an immutable reference.
    ///
    /// # Errors
    ///
    /// Returns a profile error for invalid or oversized fields, or when the
    /// exact content digest or observed version disagrees with the reference.
    pub fn from_verified_reference(
        source_id: impl Into<String>,
        content: impl Into<String>,
        observed_version: impl Into<String>,
        provenance: impl Into<String>,
        immutable_reference: VerifiedContextSourceRef,
    ) -> Result<Self, ProfileError> {
        let source_id = source_id.into();
        let content = content.into();
        let version = observed_version.into();
        let provenance = provenance.into();
        let content_digest = context_source_digest(&content)?;
        let fact = Self {
            source_id,
            content: Arc::from(content),
            content_digest,
            version,
            provenance,
            immutable_reference: Some(immutable_reference),
        };
        fact.validate()?;
        Ok(fact)
    }

    /// Returns the source identity.
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Returns the exact bounded content used in model-visible instructions.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Returns the content digest.
    #[must_use]
    pub const fn content_digest(&self) -> &Digest {
        &self.content_digest
    }

    /// Returns the observed source version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the source provenance label.
    #[must_use]
    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    /// Returns verified immutable-reference metadata when resolution used one.
    #[must_use]
    pub const fn immutable_reference(&self) -> Option<&VerifiedContextSourceRef> {
        self.immutable_reference.as_ref()
    }

    fn validate(&self) -> Result<(), ProfileError> {
        if !validate_identifier(&self.source_id) {
            return Err(ProfileError::InvalidSourceSelector);
        }
        if self.content.is_empty() || self.content.len() > MAX_INSTRUCTION_BYTES {
            return Err(ProfileError::TextTooLarge {
                max_bytes: MAX_INSTRUCTION_BYTES,
                found: self.content.len(),
            });
        }
        if !validate_source_metadata(&self.version) || !validate_source_metadata(&self.provenance) {
            return Err(ProfileError::InvalidSourceSelector);
        }
        validate_source_digest(self.content_digest)?;
        if context_source_digest(&self.content)? != self.content_digest {
            return Err(ProfileError::ContextSourceDigestMismatch {
                source_id: self.source_id.clone(),
            });
        }
        if let Some(reference) = &self.immutable_reference {
            reference.validate()?;
            if reference.version != self.version {
                return Err(ProfileError::ContextSourceVersionMismatch {
                    source_id: self.source_id.clone(),
                });
            }
            if reference.content_digest != self.content_digest {
                return Err(ProfileError::ContextSourceDigestMismatch {
                    source_id: self.source_id.clone(),
                });
            }
        }
        Ok(())
    }
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

impl CompilerModelFacts {
    /// Creates compiler model facts from an already-selected binding.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "compiler model facts are an explicit frozen input tuple"
    )]
    pub fn new(
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
    ) -> Self {
        Self {
            selected_provider_configuration_id,
            effective_provider_configuration_id,
            protocol,
            route,
            model,
            model_adapter_profile,
            model_binding_digest,
            reasoning_support,
            native_continuation_support,
            prompt_cache_support,
        }
    }

    /// Returns the complete caller-selected provider-configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration_id
    }

    /// Returns the compiled effective provider-configuration identity.
    #[must_use]
    pub const fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration_id
    }

    /// Returns the already selected route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the model-adapter profile identity.
    #[must_use]
    pub const fn model_adapter_profile(&self) -> &ModelAdapterProfileId {
        &self.model_adapter_profile
    }
}

/// Frozen tool-registry facts available to compilation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerToolFacts {
    registry_digest: Digest,
    canonical_tool_names: Vec<String>,
}

impl CompilerToolFacts {
    /// Creates tool facts from a frozen registry digest and names.
    ///
    /// # Errors
    ///
    /// Returns a profile error when a name is not portable or the list is oversized.
    pub fn new(
        registry_digest: Digest,
        canonical_tool_names: Vec<String>,
    ) -> Result<Self, ProfileError> {
        if canonical_tool_names.len() > MAX_TOOL_EXPOSURE_OVERRIDES {
            return Err(ProfileError::TooManyToolOverrides);
        }
        for name in &canonical_tool_names {
            if !validate_identifier(name) {
                return Err(ProfileError::InvalidToolOverride);
            }
        }
        Ok(Self {
            registry_digest,
            canonical_tool_names,
        })
    }

    /// Returns the registry digest.
    #[must_use]
    pub const fn registry_digest(&self) -> &Digest {
        &self.registry_digest
    }

    /// Returns canonical tool names.
    #[must_use]
    pub fn canonical_tool_names(&self) -> &[String] {
        &self.canonical_tool_names
    }
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

impl CompilerRuntimeFacts {
    /// Creates runtime facts.
    #[must_use]
    pub const fn new(
        execution_environment_digest: Digest,
        max_instruction_bytes: usize,
        require_reasoning: bool,
        require_native_continuation: bool,
        require_prompt_cache: bool,
    ) -> Self {
        Self {
            execution_environment_digest,
            max_instruction_bytes,
            require_reasoning,
            require_native_continuation,
            require_prompt_cache,
        }
    }
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

impl HarnessCompilerInputs {
    /// Creates compiler inputs.
    ///
    /// # Errors
    ///
    /// Returns a profile error when too many context sources are supplied.
    pub fn new(
        model: CompilerModelFacts,
        tools: CompilerToolFacts,
        runtime: CompilerRuntimeFacts,
        context_sources: Vec<ResolvedContextSourceFact>,
    ) -> Result<Self, ProfileError> {
        validate_context_sources(&context_sources)?;
        Ok(Self {
            selected_provider_configuration_id: model.selected_provider_configuration_id,
            effective_provider_configuration_id: model.effective_provider_configuration_id,
            model,
            tools,
            runtime,
            context_sources,
        })
    }

    /// Returns the complete caller-selected provider-configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration_id
    }

    /// Returns the compiled effective provider-configuration identity.
    #[must_use]
    pub const fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration_id
    }

    /// Returns model facts.
    #[must_use]
    pub const fn model(&self) -> &CompilerModelFacts {
        &self.model
    }

    /// Returns tool facts.
    #[must_use]
    pub const fn tools(&self) -> &CompilerToolFacts {
        &self.tools
    }
}

/// Resolved instruction plan selected by the compiler.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedInstructionPlan {
    strategy: InstructionCompositionStrategy,
    composed_instructions: String,
    sources: Vec<ResolvedContextSourceFact>,
}

impl ResolvedInstructionPlan {
    /// Returns the selected strategy.
    #[must_use]
    pub const fn strategy(&self) -> InstructionCompositionStrategy {
        self.strategy
    }

    /// Returns the composed instruction text.
    #[must_use]
    pub fn composed_instructions(&self) -> &str {
        &self.composed_instructions
    }

    /// Returns ordered, reconstruction-complete source facts bound into the plan.
    #[must_use]
    pub fn sources(&self) -> &[ResolvedContextSourceFact] {
        &self.sources
    }
}

/// Resolved tool-exposure decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedToolExposurePlan {
    strategy: ToolExposureStrategy,
    exposed_names: Vec<String>,
    overrides: Vec<ToolExposureOverride>,
}

impl ResolvedToolExposurePlan {
    /// Returns the selected strategy.
    #[must_use]
    pub const fn strategy(&self) -> ToolExposureStrategy {
        self.strategy
    }

    /// Returns model-facing exposed names in stable order.
    #[must_use]
    pub fn exposed_names(&self) -> &[String] {
        &self.exposed_names
    }

    /// Returns exact canonical-to-provider exposure overrides.
    #[must_use]
    pub fn overrides(&self) -> &[ToolExposureOverride] {
        &self.overrides
    }
}

/// Resolved reasoning decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedReasoningPlan {
    enabled: bool,
    effort: Option<ReasoningEffort>,
}

impl ResolvedReasoningPlan {
    /// Returns whether reasoning is enabled.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// Returns the selected effort.
    #[must_use]
    pub const fn effort(self) -> Option<ReasoningEffort> {
        self.effort
    }
}

/// Resolved continuation decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedContinuationPlan {
    use_native_continuation: bool,
}

impl ResolvedContinuationPlan {
    /// Returns whether native continuation was selected.
    #[must_use]
    pub const fn use_native_continuation(self) -> bool {
        self.use_native_continuation
    }
}

/// Resolved cache decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedCachePlan {
    prefer_stable_prefix: bool,
}

impl ResolvedCachePlan {
    /// Returns whether stable-prefix caching was selected.
    #[must_use]
    pub const fn prefer_stable_prefix(self) -> bool {
        self.prefer_stable_prefix
    }
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

impl ResolvedHarnessPlan {
    /// Returns the source profile identity.
    #[must_use]
    pub fn profile_id(&self) -> &HarnessProfileId {
        &self.inner.profile_id
    }

    /// Returns the profile content digest.
    #[must_use]
    pub fn profile_digest(&self) -> &Digest {
        &self.inner.profile_digest
    }

    /// Returns the resolved plan digest.
    #[must_use]
    pub fn plan_digest(&self) -> &Digest {
        &self.inner.plan_digest
    }

    /// Returns the strategy-catalog digest.
    #[must_use]
    pub fn catalog_digest(&self) -> &Digest {
        &self.inner.catalog_digest
    }

    /// Returns the compiler revision.
    #[must_use]
    pub fn compiler_revision(&self) -> u32 {
        self.inner.compiler_revision
    }

    /// Returns already-selected model facts bound into the plan.
    #[must_use]
    pub fn model_facts(&self) -> &CompilerModelFacts {
        &self.inner.model
    }

    /// Returns the complete caller-selected provider-configuration identity.
    #[must_use]
    pub fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.inner.selected_provider_configuration_id
    }

    /// Returns the compiled effective provider-configuration identity.
    #[must_use]
    pub fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.inner.effective_provider_configuration_id
    }

    /// Returns the instruction plan.
    #[must_use]
    pub fn instructions(&self) -> &ResolvedInstructionPlan {
        &self.inner.instructions
    }

    /// Returns the tool exposure plan.
    #[must_use]
    pub fn tools(&self) -> &ResolvedToolExposurePlan {
        &self.inner.tools
    }

    /// Returns the reasoning plan.
    #[must_use]
    pub fn reasoning(&self) -> ResolvedReasoningPlan {
        self.inner.reasoning
    }

    /// Returns the continuation plan.
    #[must_use]
    pub fn continuation(&self) -> ResolvedContinuationPlan {
        self.inner.continuation
    }

    /// Returns the cache plan.
    #[must_use]
    pub fn cache(&self) -> ResolvedCachePlan {
        self.inner.cache
    }

    /// Returns the exact frozen tool-registry digest used for compilation.
    #[must_use]
    pub fn tool_registry_digest(&self) -> Digest {
        self.inner.tool_registry_digest
    }

    /// Returns the exact execution-environment fact digest used for compilation.
    #[must_use]
    pub fn execution_environment_digest(&self) -> Digest {
        self.inner.execution_environment_digest
    }

    /// Returns ordered resolution diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[SafeDiagnostic] {
        &self.inner.diagnostics
    }

    pub(crate) fn source_profile(&self) -> &HarnessProfileSpec {
        &self.source.profile
    }

    pub(crate) fn source_inputs(&self) -> &HarnessCompilerInputs {
        &self.source.inputs
    }
}

/// Pure deterministic harness-profile compiler.
#[derive(Clone, Copy, Debug, Default)]
pub struct HarnessProfileCompiler;

impl HarnessProfileCompiler {
    /// Creates a compiler.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Compiles a data-only profile against frozen inputs.
    ///
    /// Compilation performs no I/O, credential access, discovery, clock read,
    /// persistence, or dispatch. Required unsupported behavior fails closed.
    /// Preferred unsupported behavior becomes an ordered diagnostic.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the specification is invalid for the inputs
    /// or a required behavior cannot be satisfied.
    pub fn compile(
        &self,
        spec: &HarnessProfileSpec,
        inputs: &HarnessCompilerInputs,
    ) -> Result<ResolvedHarnessPlan, ProfileError> {
        validate_decoded_compiler_inputs(inputs)?;
        if spec.schema_version != HARNESS_PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedSchemaVersion {
                found: spec.schema_version,
            });
        }
        if spec.base_instructions.len() > inputs.runtime.max_instruction_bytes {
            return Err(ProfileError::InstructionBudgetExceeded {
                max_bytes: inputs.runtime.max_instruction_bytes,
            });
        }

        let mut diagnostics = Vec::new();
        let profile_digest = spec.content_digest()?;
        let catalog_digest = strategy_catalog_digest()?;

        let instructions = resolve_instructions(spec, inputs, &mut diagnostics)?;
        let tools = resolve_tools(spec, inputs)?;
        let reasoning = resolve_reasoning(spec, inputs, &mut diagnostics)?;
        let continuation = resolve_continuation(spec, inputs, &mut diagnostics)?;
        let cache = resolve_cache(spec, inputs, &mut diagnostics)?;

        if diagnostics.len() > MAX_DIAGNOSTICS {
            return Err(ProfileError::TooManyDiagnostics);
        }

        let inner = ResolvedHarnessPlanInner {
            profile_id: spec.id.clone(),
            profile_digest,
            plan_digest: Digest::sha256(
                DigestDomain::ResolvedHarnessPlan,
                PLAN_DIGEST_VERSION,
                &[],
            )
            .map_err(ProfileError::from)?, // placeholder replaced below
            catalog_digest,
            compiler_revision: HARNESS_PROFILE_COMPILER_REVISION,
            strategy_catalog_revision: HARNESS_STRATEGY_CATALOG_REVISION,
            schema_version: spec.schema_version,
            semantic_version: spec.semantic_version,
            provenance: spec.provenance,
            selected_provider_configuration_id: inputs.selected_provider_configuration_id,
            effective_provider_configuration_id: inputs.effective_provider_configuration_id,
            model: inputs.model.clone(),
            tool_registry_digest: inputs.tools.registry_digest,
            execution_environment_digest: inputs.runtime.execution_environment_digest,
            instructions,
            tools,
            reasoning,
            continuation,
            cache,
            diagnostics,
        };

        let plan_digest = plan_content_digest(&inner)?;
        let mut sealed = inner;
        sealed.plan_digest = plan_digest;

        Ok(ResolvedHarnessPlan {
            inner: Arc::new(sealed),
            source: Arc::new(ResolvedHarnessPlanSource {
                profile: spec.clone(),
                inputs: inputs.clone(),
            }),
        })
    }
}

fn resolve_instructions(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedInstructionPlan, ProfileError> {
    let mut sources = Vec::new();
    let mut composed = spec.base_instructions.clone();

    match spec.instruction_strategy {
        InstructionCompositionStrategy::BaseOnly => {
            if !spec.instruction_sources.is_empty() {
                diagnostics.push(SafeDiagnostic::new(
                    "instruction sources ignored under BaseOnly strategy",
                ));
            }
        }
        InstructionCompositionStrategy::BaseThenSources => {
            for selector in &spec.instruction_sources {
                let Some(fact) = inputs
                    .context_sources
                    .iter()
                    .find(|fact| fact.source_id() == selector.source_id())
                else {
                    return Err(ProfileError::UnresolvedContextSource {
                        source_id: selector.source_id().to_string(),
                    });
                };
                let separator_bytes = usize::from(!composed.is_empty()) * 2;
                let next_len = composed
                    .len()
                    .checked_add(separator_bytes)
                    .and_then(|length| length.checked_add(fact.content().len()))
                    .ok_or(ProfileError::InstructionBudgetExceeded {
                        max_bytes: inputs.runtime.max_instruction_bytes,
                    })?;
                if next_len > inputs.runtime.max_instruction_bytes {
                    return Err(ProfileError::InstructionBudgetExceeded {
                        max_bytes: inputs.runtime.max_instruction_bytes,
                    });
                }
                if separator_bytes != 0 {
                    composed.push_str("\n\n");
                }
                composed.push_str(fact.content());
                sources.push(fact.clone());
            }
        }
    }

    if composed.len() > inputs.runtime.max_instruction_bytes {
        return Err(ProfileError::InstructionBudgetExceeded {
            max_bytes: inputs.runtime.max_instruction_bytes,
        });
    }

    Ok(ResolvedInstructionPlan {
        strategy: spec.instruction_strategy,
        composed_instructions: composed,
        sources,
    })
}

fn resolve_tools(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
) -> Result<ResolvedToolExposurePlan, ProfileError> {
    if spec.tool_exposure_strategy == ToolExposureStrategy::None && !spec.tool_overrides.is_empty()
    {
        return Err(ProfileError::InvalidToolOverride);
    }
    let mut exposed_names = Vec::new();
    match spec.tool_exposure_strategy {
        ToolExposureStrategy::None => {}
        ToolExposureStrategy::CanonicalAll => {
            exposed_names.extend(inputs.tools.canonical_tool_names.iter().cloned());
        }
    }

    for override_entry in &spec.tool_overrides {
        if !inputs
            .tools
            .canonical_tool_names
            .iter()
            .any(|name| name == override_entry.canonical_name())
        {
            return Err(ProfileError::UnknownCanonicalTool {
                name: override_entry.canonical_name().to_string(),
            });
        }
        if let Some(index) = exposed_names
            .iter()
            .position(|name| name == override_entry.canonical_name())
        {
            exposed_names[index] = override_entry.exposed_name().to_string();
        }
    }

    Ok(ResolvedToolExposurePlan {
        strategy: spec.tool_exposure_strategy,
        exposed_names,
        overrides: spec.tool_overrides.clone(),
    })
}

fn resolve_reasoning(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedReasoningPlan, ProfileError> {
    match spec.reasoning_strategy {
        ReasoningPreferenceStrategy::Disabled => {
            if inputs.runtime.require_reasoning {
                return Err(ProfileError::RequiredFeatureUnavailable {
                    feature: "reasoning",
                });
            }
            Ok(ResolvedReasoningPlan {
                enabled: false,
                effort: None,
            })
        }
        ReasoningPreferenceStrategy::PreferEffort => match inputs.model.reasoning_support {
            FeatureSupportFact::Supported => Ok(ResolvedReasoningPlan {
                enabled: true,
                effort: spec.preferred_reasoning_effort,
            }),
            FeatureSupportFact::Unknown | FeatureSupportFact::Unsupported => {
                if inputs.runtime.require_reasoning {
                    return Err(ProfileError::RequiredFeatureUnavailable {
                        feature: "reasoning",
                    });
                }
                diagnostics.push(SafeDiagnostic::new(
                    "preferred reasoning effort downgraded: model support is not available",
                ));
                Ok(ResolvedReasoningPlan {
                    enabled: false,
                    effort: None,
                })
            }
        },
    }
}

fn resolve_continuation(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedContinuationPlan, ProfileError> {
    match spec.continuation_strategy {
        ContinuationPreferenceStrategy::CanonicalReplayOnly => {
            if inputs.runtime.require_native_continuation {
                return Err(ProfileError::RequiredFeatureUnavailable {
                    feature: "native_continuation",
                });
            }
            Ok(ResolvedContinuationPlan {
                use_native_continuation: false,
            })
        }
        ContinuationPreferenceStrategy::PreferNative => {
            match inputs.model.native_continuation_support {
                FeatureSupportFact::Supported => Ok(ResolvedContinuationPlan {
                    use_native_continuation: true,
                }),
                FeatureSupportFact::Unknown | FeatureSupportFact::Unsupported => {
                    if inputs.runtime.require_native_continuation {
                        return Err(ProfileError::RequiredFeatureUnavailable {
                            feature: "native_continuation",
                        });
                    }
                    diagnostics.push(SafeDiagnostic::new(
                        "preferred native continuation downgraded to canonical replay",
                    ));
                    Ok(ResolvedContinuationPlan {
                        use_native_continuation: false,
                    })
                }
            }
        }
    }
}

fn resolve_cache(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedCachePlan, ProfileError> {
    match spec.cache_strategy {
        CachePreferenceStrategy::Disabled => {
            if inputs.runtime.require_prompt_cache {
                return Err(ProfileError::RequiredFeatureUnavailable {
                    feature: "prompt_cache",
                });
            }
            Ok(ResolvedCachePlan {
                prefer_stable_prefix: false,
            })
        }
        CachePreferenceStrategy::PreferStablePrefix => match inputs.model.prompt_cache_support {
            FeatureSupportFact::Supported => Ok(ResolvedCachePlan {
                prefer_stable_prefix: true,
            }),
            FeatureSupportFact::Unknown | FeatureSupportFact::Unsupported => {
                if inputs.runtime.require_prompt_cache {
                    return Err(ProfileError::RequiredFeatureUnavailable {
                        feature: "prompt_cache",
                    });
                }
                diagnostics.push(SafeDiagnostic::new(
                    "preferred prompt cache downgraded: model support is not available",
                ));
                Ok(ResolvedCachePlan {
                    prefer_stable_prefix: false,
                })
            }
        },
    }
}

fn strategy_catalog_digest() -> Result<Digest, ProfileError> {
    let parts: &[&[u8]] = &[
        b"instruction:BaseThenSources|BaseOnly",
        b"tools:CanonicalAll|None",
        b"reasoning:Disabled|PreferEffort",
        b"continuation:PreferNative|CanonicalReplayOnly",
        b"cache:PreferStablePrefix|Disabled",
        &HARNESS_STRATEGY_CATALOG_REVISION.to_be_bytes(),
    ];
    Digest::sha256(
        DigestDomain::HarnessStrategyCatalog,
        CATALOG_DIGEST_VERSION,
        parts,
    )
    .map_err(ProfileError::from)
}

fn plan_content_digest(inner: &ResolvedHarnessPlanInner) -> Result<Digest, ProfileError> {
    // Digest excludes plan_digest itself; use a zeroed placeholder field value.
    let mut for_digest = inner.clone();
    for_digest.plan_digest = Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        PLAN_DIGEST_VERSION,
        &[b"placeholder"],
    )
    .map_err(ProfileError::from)?;
    let bytes = serde_json::to_vec(&for_digest).map_err(|error| ProfileError::Serialization {
        reason: error.to_string(),
    })?;
    Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        PLAN_DIGEST_VERSION,
        &[
            b"resolved-harness-plan.v2",
            &bytes,
            &HARNESS_PROFILE_COMPILER_REVISION.to_be_bytes(),
        ],
    )
    .map_err(ProfileError::from)
}

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

/// Errors constructing or compiling harness profiles.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProfileError {
    /// Profile identity is missing.
    #[error("harness profile identity is required")]
    MissingIdentity,
    /// Profile identity is invalid.
    #[error("harness profile identity is invalid")]
    InvalidIdentity,
    /// Context-source selector is invalid.
    #[error("context source selector is invalid")]
    InvalidSourceSelector,
    /// Tool override is invalid.
    #[error("tool exposure override is invalid")]
    InvalidToolOverride,
    /// Text field exceeds its bound.
    #[error("profile text exceeds {max_bytes} bytes (found {found})")]
    TextTooLarge {
        /// Maximum permitted bytes.
        max_bytes: usize,
        /// Observed byte length.
        found: usize,
    },
    /// Too many instruction sources.
    #[error("too many instruction sources")]
    TooManySources,
    /// Too many tool overrides.
    #[error("too many tool exposure overrides")]
    TooManyToolOverrides,
    /// Too many diagnostics.
    #[error("too many profile diagnostics")]
    TooManyDiagnostics,
    /// `PreferEffort` without an effort value.
    #[error("preferred reasoning effort is required")]
    MissingReasoningEffort,
    /// Effort supplied while reasoning is disabled.
    #[error("reasoning effort is not allowed when reasoning is disabled")]
    UnexpectedReasoningEffort,
    /// Unsupported schema version.
    #[error("unsupported harness profile schema version {found}")]
    UnsupportedSchemaVersion {
        /// Observed schema version.
        found: u32,
    },
    /// Instruction composition exceeded the runtime budget.
    #[error("composed instructions exceed the {max_bytes}-byte budget")]
    InstructionBudgetExceeded {
        /// Maximum permitted instruction bytes.
        max_bytes: usize,
    },
    /// A selected context source was not resolved before compilation.
    #[error("context source `{source_id}` was not resolved before compilation")]
    UnresolvedContextSource {
        /// Missing source identity.
        source_id: String,
    },
    /// Tool override names a tool absent from the frozen registry.
    #[error("canonical tool `{name}` is not present in the frozen registry")]
    UnknownCanonicalTool {
        /// Missing tool name.
        name: String,
    },
    /// Required feature cannot be satisfied by model/runtime facts.
    #[error("required feature `{feature}` is unavailable")]
    RequiredFeatureUnavailable {
        /// Feature name.
        feature: &'static str,
    },
    /// Context-source digest domain is not accepted.
    #[error("context source digest domain is invalid")]
    InvalidSourceDigestDomain,
    /// Immutable context-source reference is invalid.
    #[error("immutable context source reference is invalid")]
    InvalidContextSourceReference,
    /// Exact context-source content disagrees with its recorded digest.
    #[error("context source `{source_id}` content digest does not match")]
    ContextSourceDigestMismatch {
        /// Source whose exact content failed verification.
        source_id: String,
    },
    /// Observed context-source version disagrees with its immutable reference.
    #[error("context source `{source_id}` version does not match")]
    ContextSourceVersionMismatch {
        /// Source whose observed version failed verification.
        source_id: String,
    },
    /// A typed compiler authority fact carries a foreign digest domain.
    #[error("compiler input digest domain is invalid")]
    InvalidCompilerDigestDomain,
    /// Serialization failure while digesting profile or plan data.
    #[error("profile serialization failed: {reason}")]
    Serialization {
        /// Failure reason.
        reason: String,
    },
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// JSON profile decoding failed closed.
    #[error("harness profile JSON is invalid: {reason}")]
    InvalidJson {
        /// Failure reason.
        reason: String,
    },
    /// Context-source catalog already contains the identity.
    #[error("context source `{source_id}` is already registered")]
    DuplicateContextSource {
        /// Conflicting source identity.
        source_id: String,
    },
    /// Explicit plan binding does not match the selected model adapter facts.
    #[error("resolved harness plan is bound to a different model/route/adapter")]
    PlanModelBindingMismatch,
    /// Explicit plan tool exposure disagrees with the provided exposure map.
    #[error("resolved harness plan tool exposure disagrees with the tool exposure map")]
    PlanToolExposureMismatch,
}

impl ProfileError {
    /// Decodes a profile specification from JSON with unknown-field rejection.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::InvalidJson`] or a validation error.
    pub fn decode_spec(bytes: &[u8]) -> Result<HarnessProfileSpec, Self> {
        let spec: HarnessProfileSpec =
            serde_json::from_slice(bytes).map_err(|error| Self::InvalidJson {
                reason: error.to_string(),
            })?;
        validate_decoded_spec(&spec)?;
        Ok(spec)
    }
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

impl ResolvedHarnessPlan {
    /// Converts the resolved instruction plan into canonical instruction blocks.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the composed text is empty or oversized for
    /// the portable instruction block budget.
    pub fn instruction_blocks(&self) -> Result<Vec<InstructionBlock>, ProfileError> {
        let text = self.instructions().composed_instructions();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        InstructionBlock::new(InstructionSource::Profile, text)
            .map(|block| vec![block])
            .map_err(|error| ProfileError::Serialization {
                reason: error.to_string(),
            })
    }

    /// Converts the resolved reasoning plan into portable reasoning policy.
    #[must_use]
    pub fn reasoning_policy(&self) -> ReasoningPolicy {
        // Required-or-omit: PreferEffort already decided enable vs
        // omit at plan compile time. When enabled, emit Required (not Preferred):
        // under Strict prepare, Preferred is fatal and is not a soft preference.
        if self.reasoning().enabled() {
            ReasoningPolicy::Enabled {
                effort: self.reasoning().effort(),
                requirement: CapabilityRequirement::Required,
            }
        } else {
            ReasoningPolicy::Disabled
        }
    }

    /// Returns whether native continuation may be offered to the model.
    #[must_use]
    pub fn allows_native_continuation(&self) -> bool {
        self.continuation().use_native_continuation()
    }

    /// Validates that this plan is bound to the same model/route/adapter facts.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::PlanModelBindingMismatch`] on disagreement.
    pub fn ensure_model_binding(&self, model: &CompilerModelFacts) -> Result<(), ProfileError> {
        let bound = self.model_facts();
        if bound.selected_provider_configuration_id() != self.selected_provider_configuration_id()
            || bound.effective_provider_configuration_id()
                != self.effective_provider_configuration_id()
            || bound.route() != model.route()
            || bound.model_adapter_profile() != model.model_adapter_profile()
            || self.selected_provider_configuration_id()
                != model.selected_provider_configuration_id()
            || self.effective_provider_configuration_id()
                != model.effective_provider_configuration_id()
            || bound.protocol != model.protocol
            || bound.model != model.model
            || bound.model_binding_digest != model.model_binding_digest
        {
            return Err(ProfileError::PlanModelBindingMismatch);
        }
        Ok(())
    }
}

/// Maps descriptor support into compiler support facts without inventing support.
#[must_use]
pub fn feature_support_from_descriptor(
    descriptor: &ResolvedModelDescriptor,
    capability: Capability,
) -> FeatureSupportFact {
    match descriptor
        .capabilities()
        .get(capability)
        .map(plakous_model::FeatureSupport::support)
    {
        Some(Support::Native | Support::Emulated | Support::Degraded) => {
            FeatureSupportFact::Supported
        }
        Some(Support::Unsupported) => FeatureSupportFact::Unsupported,
        Some(_) | None => FeatureSupportFact::Unknown,
    }
}

/// Builds pure compiler model facts from an already-selected language model.
///
/// # Errors
///
/// Returns a profile error when the model descriptor cannot be digested.
pub fn compiler_model_facts_from_model(
    _model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
) -> Result<CompilerModelFacts, ProfileError> {
    let descriptor = provider_configuration.effective().descriptor();
    let binding =
        descriptor
            .semantic_binding_digest()
            .map_err(|error| ProfileError::Serialization {
                reason: error.to_string(),
            })?;
    let behavior = provider_configuration.effective().behavior();
    let reasoning_support = match behavior.reasoning_effort {
        ReasoningEffortDisposition::Disabled => FeatureSupportFact::Unsupported,
        ReasoningEffortDisposition::Enabled(_) => {
            feature_support_from_descriptor(descriptor, Capability::Reasoning)
        }
    };
    let prompt_cache_support = match behavior.cache {
        CachePolicy::Enabled { .. } => {
            feature_support_from_descriptor(descriptor, Capability::Caching)
        }
        _ => FeatureSupportFact::Unsupported,
    };
    Ok(CompilerModelFacts::new(
        provider_configuration.selected_identity(),
        provider_configuration.effective_identity(),
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        descriptor.profile().clone(),
        binding,
        reasoning_support,
        feature_support_from_descriptor(descriptor, Capability::Continuation),
        prompt_cache_support,
    ))
}

/// Builds tool facts from a frozen registry digest and canonical names.
///
/// # Errors
///
/// Returns a profile error for invalid names.
pub fn compiler_tool_facts(
    registry_digest: Digest,
    canonical_tool_names: Vec<String>,
) -> Result<CompilerToolFacts, ProfileError> {
    CompilerToolFacts::new(registry_digest, canonical_tool_names)
}

/// Compiles a construction-bound plan from explicit builder policy.
///
/// Used when a caller has not supplied a precompiled plan: the agent still
/// binds an immutable plan digest derived only from the construction-time
/// instruction/reasoning/tool selections already chosen by the caller.
///
/// # Errors
///
/// Returns a profile error when the derived specification cannot compile.
#[allow(
    clippy::too_many_arguments,
    reason = "construction-bound plan compilation takes one explicit frozen authority tuple"
)]
pub fn compile_construction_bound_plan(
    model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    registry_digest: Digest,
    canonical_tool_names: Vec<String>,
    execution_environment_digest: Digest,
    instructions: &[InstructionBlock],
    reasoning: ReasoningPolicy,
    prefer_native_continuation: bool,
) -> Result<ResolvedHarnessPlan, ProfileError> {
    let base_instructions = if instructions.is_empty() {
        String::new()
    } else {
        instructions
            .iter()
            .map(InstructionBlock::text)
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let (reasoning_strategy, preferred_effort) = match reasoning {
        ReasoningPolicy::Enabled { effort, .. } => (
            ReasoningPreferenceStrategy::PreferEffort,
            effort.or(Some(ReasoningEffort::Medium)),
        ),
        ReasoningPolicy::Disabled | _ => (ReasoningPreferenceStrategy::Disabled, None),
    };

    let continuation = if prefer_native_continuation {
        ContinuationPreferenceStrategy::PreferNative
    } else {
        ContinuationPreferenceStrategy::CanonicalReplayOnly
    };

    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plakous.construction-bound.v1")?)
        .provenance(HarnessProfileProvenance::Operator)
        .display_name("Construction-bound plan")
        .base_instructions(base_instructions)
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
        .reasoning_strategy(reasoning_strategy)
        .preferred_reasoning_effort(preferred_effort)
        .continuation_strategy(continuation)
        .cache_strategy(CachePreferenceStrategy::PreferStablePrefix)
        .build()?;

    let tools = CompilerToolFacts::new(registry_digest, canonical_tool_names)?;
    let runtime = CompilerRuntimeFacts::new(
        execution_environment_digest,
        MAX_INSTRUCTION_BYTES,
        false,
        false,
        false,
    );
    let inputs = HarnessCompilerInputs::new(
        compiler_model_facts_from_model(model, provider_configuration)?,
        tools,
        runtime,
        Vec::new(),
    )?;
    HarnessProfileCompiler::new().compile(&spec, &inputs)
}

/// Compiles an explicit profile against the selected model and frozen tools.
///
/// # Errors
///
/// Returns a profile or compilation error.
pub fn compile_explicit_profile(
    spec: &HarnessProfileSpec,
    model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    registry_digest: Digest,
    canonical_tool_names: Vec<String>,
    execution_environment_digest: Digest,
    context_sources: Vec<ResolvedContextSourceFact>,
) -> Result<ResolvedHarnessPlan, ProfileError> {
    let tools = CompilerToolFacts::new(registry_digest, canonical_tool_names)?;
    let runtime = CompilerRuntimeFacts::new(
        execution_environment_digest,
        MAX_INSTRUCTION_BYTES,
        false,
        false,
        false,
    );
    let inputs = HarnessCompilerInputs::new(
        compiler_model_facts_from_model(model, provider_configuration)?,
        tools,
        runtime,
        context_sources,
    )?;
    HarnessProfileCompiler::new().compile(spec, &inputs)
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
mod tests {
    use super::*;
    use plakous_model::{
        Digest, DigestDomain, EffectiveProviderConfigurationId, SelectedProviderConfigurationId,
    };

    fn provider_configuration_ids(
        tag: &[u8],
    ) -> (
        SelectedProviderConfigurationId,
        EffectiveProviderConfigurationId,
    ) {
        let typed = |domain| {
            let digest = Digest::sha256(domain, 5, &[tag]).expect("configuration identity");
            serde_json::to_value(digest).expect("identity wire")
        };
        (
            serde_json::from_value(typed(DigestDomain::SelectedProviderConfiguration))
                .expect("selected identity"),
            serde_json::from_value(typed(DigestDomain::EffectiveProviderConfiguration))
                .expect("effective identity"),
        )
    }

    fn model_facts(support: FeatureSupportFact) -> CompilerModelFacts {
        let (selected, effective) = provider_configuration_ids(b"configuration");
        CompilerModelFacts::new(
            selected,
            effective,
            ProtocolId::new("test.protocol").expect("protocol"),
            RouteId::new("test.route").expect("route"),
            ModelId::new("test-model").expect("model"),
            ModelAdapterProfileId::new("test.adapter.profile").expect("adapter"),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"]).expect("binding"),
            support,
            support,
            support,
        )
    }

    fn tool_facts(names: &[&str]) -> CompilerToolFacts {
        CompilerToolFacts::new(
            Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"registry"]).expect("tools"),
            names.iter().map(|name| (*name).to_owned()).collect(),
        )
        .expect("tool facts")
    }

    fn runtime_facts(require_all: bool) -> CompilerRuntimeFacts {
        CompilerRuntimeFacts::new(
            Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"env"]).expect("env"),
            MAX_INSTRUCTION_BYTES,
            require_all,
            require_all,
            require_all,
        )
    }

    fn inputs(
        support: FeatureSupportFact,
        require_all: bool,
        tools: &[&str],
    ) -> HarnessCompilerInputs {
        HarnessCompilerInputs::new(
            model_facts(support),
            tool_facts(tools),
            runtime_facts(require_all),
            Vec::new(),
        )
        .expect("inputs")
    }

    fn inputs_with_sources(
        support: FeatureSupportFact,
        sources: Vec<ResolvedContextSourceFact>,
    ) -> HarnessCompilerInputs {
        HarnessCompilerInputs::new(
            model_facts(support),
            tool_facts(&[]),
            runtime_facts(false),
            sources,
        )
        .expect("inputs")
    }

    fn synthetic_spec(id: &str, reasoning: ReasoningPreferenceStrategy) -> HarnessProfileSpec {
        let mut builder = HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new(id).expect("id"))
            .base_instructions("be precise")
            .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
            .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
            .cache_strategy(CachePreferenceStrategy::PreferStablePrefix)
            .reasoning_strategy(reasoning);
        if reasoning == ReasoningPreferenceStrategy::PreferEffort {
            builder = builder.preferred_reasoning_effort(Some(ReasoningEffort::Medium));
        }
        builder.build().expect("spec")
    }

    #[test]
    fn compile_is_deterministic_for_identical_inputs() {
        let compiler = HarnessProfileCompiler::new();
        let spec = synthetic_spec("plan.a", ReasoningPreferenceStrategy::PreferEffort);
        let inputs = inputs(
            FeatureSupportFact::Supported,
            false,
            &["read_file", "shell"],
        );
        let left = compiler.compile(&spec, &inputs).expect("left");
        let right = compiler.compile(&spec, &inputs).expect("right");
        assert_eq!(left.plan_digest(), right.plan_digest());
        assert_eq!(left.profile_digest(), right.profile_digest());
        assert_eq!(
            left.instructions().composed_instructions(),
            right.instructions().composed_instructions()
        );
    }

    #[test]
    fn plan_model_binding_still_compares_every_identity_axis_exactly() {
        let spec = synthetic_spec("plan.exact-binding", ReasoningPreferenceStrategy::Disabled);
        let inputs = inputs(FeatureSupportFact::Supported, false, &[]);
        let plan = HarnessProfileCompiler::new()
            .compile(&spec, &inputs)
            .expect("plan");
        let model = inputs.model().clone();
        assert_eq!(plan.ensure_model_binding(&model), Ok(()));
        for axis in [
            "route",
            "model",
            "protocol",
            "adapter",
            "binding",
            "selected",
            "effective",
        ] {
            let mut changed = model.clone();
            match axis {
                "route" => changed.route = RouteId::new("different.route").expect("route"),
                "model" => changed.model = ModelId::new("different-model").expect("model"),
                "protocol" => {
                    changed.protocol = ProtocolId::new("different.protocol").expect("protocol");
                }
                "adapter" => {
                    changed.model_adapter_profile =
                        ModelAdapterProfileId::new("different.adapter").expect("adapter");
                }
                "binding" => {
                    changed.model_binding_digest =
                        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"different-binding"])
                            .expect("binding");
                }
                "selected" => {
                    changed.selected_provider_configuration_id =
                        provider_configuration_ids(b"different").0;
                }
                _ => {
                    changed.effective_provider_configuration_id =
                        provider_configuration_ids(b"different").1;
                }
            }
            assert_eq!(
                plan.ensure_model_binding(&changed),
                Err(ProfileError::PlanModelBindingMismatch),
                "{axis}"
            );
        }
    }

    #[test]
    fn provider_configuration_ids_are_required_plan_authority() {
        let compiler = HarnessProfileCompiler::new();
        let spec = synthetic_spec("plan.configuration", ReasoningPreferenceStrategy::Disabled);
        let baseline_inputs = inputs(FeatureSupportFact::Supported, false, &[]);
        let baseline = compiler.compile(&spec, &baseline_inputs).expect("baseline");
        assert_eq!(
            baseline.selected_provider_configuration_id(),
            baseline_inputs.selected_provider_configuration_id()
        );
        assert_eq!(
            baseline.effective_provider_configuration_id(),
            baseline_inputs.effective_provider_configuration_id()
        );

        let (other_selected, other_effective) = provider_configuration_ids(b"other");
        let mut selected_model = model_facts(FeatureSupportFact::Supported);
        selected_model.selected_provider_configuration_id = other_selected;
        let selected_inputs = HarnessCompilerInputs::new(
            selected_model,
            tool_facts(&[]),
            runtime_facts(false),
            Vec::new(),
        )
        .expect("selected inputs");
        let selected_plan = compiler
            .compile(&spec, &selected_inputs)
            .expect("selected plan");
        assert_ne!(baseline.plan_digest(), selected_plan.plan_digest());
        assert_eq!(
            baseline.ensure_model_binding(selected_inputs.model()),
            Err(ProfileError::PlanModelBindingMismatch)
        );

        let mut effective_model = model_facts(FeatureSupportFact::Supported);
        effective_model.effective_provider_configuration_id = other_effective;
        let effective_inputs = HarnessCompilerInputs::new(
            effective_model,
            tool_facts(&[]),
            runtime_facts(false),
            Vec::new(),
        )
        .expect("effective inputs");
        let effective_plan = compiler
            .compile(&spec, &effective_inputs)
            .expect("effective plan");
        assert_ne!(baseline.plan_digest(), effective_plan.plan_digest());
        assert_eq!(
            baseline.ensure_model_binding(effective_inputs.model()),
            Err(ProfileError::PlanModelBindingMismatch)
        );

        let canonical_inputs = String::from_utf8(
            canonical_compiler_input_bytes(&baseline_inputs).expect("canonical inputs"),
        )
        .expect("utf8 inputs");
        let canonical_plan =
            String::from_utf8(canonical_plan_bytes(&baseline).expect("canonical plan"))
                .expect("utf8 plan");
        for field in [
            "selected_provider_configuration_id",
            "effective_provider_configuration_id",
        ] {
            assert!(canonical_inputs.contains(field));
            assert!(canonical_plan.contains(field));
        }
    }

    #[test]
    fn request_affecting_change_changes_digests() {
        let compiler = HarnessProfileCompiler::new();
        let left_spec = synthetic_spec("plan.a", ReasoningPreferenceStrategy::PreferEffort);
        let right_spec = HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plan.a").expect("id"))
            .base_instructions("be precise and thorough")
            .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
            .preferred_reasoning_effort(Some(ReasoningEffort::Medium))
            .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
            .cache_strategy(CachePreferenceStrategy::PreferStablePrefix)
            .build()
            .expect("spec");
        let inputs = inputs(FeatureSupportFact::Supported, false, &["read_file"]);
        let left = compiler.compile(&left_spec, &inputs).expect("left");
        let right = compiler.compile(&right_spec, &inputs).expect("right");
        assert_ne!(left.profile_digest(), right.profile_digest());
        assert_ne!(left.plan_digest(), right.plan_digest());
    }

    #[test]
    fn exact_context_content_is_model_visible_and_changes_plan_identity() {
        let spec = HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plan.context").expect("id"))
            .base_instructions("base policy")
            .instruction_strategy(InstructionCompositionStrategy::BaseThenSources)
            .instruction_sources(vec![
                ContextSourceSelector::new("repo.agents").expect("selector"),
            ])
            .build()
            .expect("spec");
        let exact_digest =
            Digest::sha256(DigestDomain::Blob, 1, &[b"exact source A"]).expect("digest");
        let immutable_reference =
            VerifiedContextSourceRef::new("object.repo-agents", exact_digest, "1")
                .expect("reference");
        let left_source = ResolvedContextSourceFact::from_verified_reference(
            "repo.agents",
            "exact source A",
            "1",
            "test",
            immutable_reference,
        )
        .expect("source");
        let right_source =
            ResolvedContextSourceFact::new("repo.agents", "exact source B", "1", "test")
                .expect("source");
        let compiler = HarnessProfileCompiler::new();
        let left = compiler
            .compile(
                &spec,
                &inputs_with_sources(FeatureSupportFact::Supported, vec![left_source.clone()]),
            )
            .expect("left");
        let right = compiler
            .compile(
                &spec,
                &inputs_with_sources(FeatureSupportFact::Supported, vec![right_source]),
            )
            .expect("right");

        assert_eq!(
            left.instructions().composed_instructions(),
            "base policy\n\nexact source A"
        );
        assert_eq!(left.instructions().sources(), &[left_source]);
        assert_eq!(
            left.instructions().sources()[0]
                .immutable_reference()
                .expect("reference")
                .reference_id(),
            "object.repo-agents"
        );
        assert_eq!(
            left.instruction_blocks()
                .expect("instruction blocks")
                .first()
                .expect("model-visible block")
                .text(),
            "base policy\n\nexact source A"
        );
        assert!(
            !left
                .instructions()
                .composed_instructions()
                .contains("[source:")
        );
        assert_ne!(left.plan_digest(), right.plan_digest());
    }

    #[test]
    fn duplicate_resolved_context_id_fails_closed() {
        let first =
            ResolvedContextSourceFact::new("repo.agents", "one", "1", "test").expect("first");
        let second =
            ResolvedContextSourceFact::new("repo.agents", "two", "2", "test").expect("second");
        let error = HarnessCompilerInputs::new(
            model_facts(FeatureSupportFact::Supported),
            tool_facts(&[]),
            runtime_facts(false),
            vec![first, second],
        )
        .expect_err("duplicate");
        assert!(matches!(error, ProfileError::DuplicateContextSource { .. }));
    }

    #[test]
    fn aggregate_resolved_context_bound_fails_closed() {
        let content = "x".repeat((MAX_INSTRUCTION_BYTES / 2) + 1);
        let first =
            ResolvedContextSourceFact::new("repo.one", &content, "1", "test").expect("first");
        let second =
            ResolvedContextSourceFact::new("repo.two", content, "1", "test").expect("second");
        let error = HarnessCompilerInputs::new(
            model_facts(FeatureSupportFact::Supported),
            tool_facts(&[]),
            runtime_facts(false),
            vec![first, second],
        )
        .expect_err("aggregate bound");
        assert!(matches!(
            error,
            ProfileError::InstructionBudgetExceeded { .. }
        ));
    }

    #[test]
    fn tampered_deserialized_context_content_fails_closed() {
        let source =
            ResolvedContextSourceFact::new("repo.agents", "trusted", "1", "test").expect("source");
        let mut value = serde_json::to_value(source).expect("serialize");
        value["content"] = serde_json::Value::String("tampered".to_owned());
        let tampered: ResolvedContextSourceFact =
            serde_json::from_value(value).expect("structural decode");
        let error = HarnessCompilerInputs::new(
            model_facts(FeatureSupportFact::Supported),
            tool_facts(&[]),
            runtime_facts(false),
            vec![tampered],
        )
        .expect_err("digest mismatch");
        assert!(matches!(
            error,
            ProfileError::ContextSourceDigestMismatch { .. }
        ));
    }

    #[test]
    fn preferred_unsupported_features_downgrade_with_diagnostics() {
        let compiler = HarnessProfileCompiler::new();
        let spec = synthetic_spec("plan.soft", ReasoningPreferenceStrategy::PreferEffort);
        let plan = compiler
            .compile(&spec, &inputs(FeatureSupportFact::Unsupported, false, &[]))
            .expect("plan");
        assert!(!plan.reasoning().enabled());
        assert!(!plan.continuation().use_native_continuation());
        assert!(!plan.cache().prefer_stable_prefix());
        assert!(plan.diagnostics().len() >= 3);
    }

    #[test]
    fn required_unsupported_features_fail_closed() {
        let compiler = HarnessProfileCompiler::new();
        let spec = synthetic_spec("plan.hard", ReasoningPreferenceStrategy::PreferEffort);
        let error = compiler
            .compile(&spec, &inputs(FeatureSupportFact::Unsupported, true, &[]))
            .expect_err("must fail");
        assert!(matches!(
            error,
            ProfileError::RequiredFeatureUnavailable { .. }
        ));
    }

    #[test]
    fn deleted_profile_axes_are_rejected_as_unknown_fields() {
        let profile = HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("serde.axes").expect("id"))
            .base_instructions("active")
            .build()
            .expect("profile");
        for (field, value) in [
            ("context_strategy", serde_json::json!("ManualOnly")),
            ("verbosity_strategy", serde_json::json!("Concise")),
            ("steering_template", serde_json::json!("steer")),
            ("recovery_template", serde_json::json!("recover")),
        ] {
            let mut wire = serde_json::to_value(&profile).expect("wire");
            wire.as_object_mut()
                .expect("profile object")
                .insert(field.to_owned(), value);
            let bytes = serde_json::to_vec(&wire).expect("bytes");
            let error = ProfileError::decode_spec(&bytes).expect_err("deleted field");
            assert!(matches!(error, ProfileError::InvalidJson { .. }));
        }
    }

    #[test]
    fn canonical_encodings_omit_deferred_axes() {
        let spec = synthetic_spec("serde.active", ReasoningPreferenceStrategy::Disabled);
        let inputs = inputs(FeatureSupportFact::Supported, false, &[]);
        let plan = HarnessProfileCompiler::new()
            .compile(&spec, &inputs)
            .expect("plan");
        let plan_encoding =
            String::from_utf8(canonical_plan_bytes(&plan).expect("plan bytes")).expect("utf8");
        let encoded = [
            serde_json::to_string(&spec).expect("profile"),
            serde_json::to_string(&inputs).expect("inputs"),
            plan_encoding.clone(),
        ]
        .join("\n");

        for deleted in [
            "context_strategy",
            "verbosity_strategy",
            "steering_template",
            "recovery_template",
            "automatic_compaction_support",
            "require_automatic_compaction",
            "\"context\":",
            "\"output\":",
        ] {
            assert!(!encoded.contains(deleted), "retained {deleted}");
        }
        assert!(encoded.contains("continuation_strategy"));
        assert!(encoded.contains("cache_strategy"));
        assert!(plan_encoding.contains("\"use_native_continuation\":true"));
        assert!(plan_encoding.contains("\"prefer_stable_prefix\":true"));
    }

    #[test]
    fn profiles_cannot_satisfy_model_adapter_or_subscription_identities() {
        // Compile-time non-coercion is structural: the three ID types are distinct.
        // Runtime proof: digests and plan bindings keep model-adapter identity separate
        // from harness-profile identity even when display strings collide.
        let harness = HarnessProfileId::new("same-display-name").expect("harness");
        let adapter = ModelAdapterProfileId::new("same-display-name").expect("adapter");
        assert_eq!(harness.as_str(), adapter.as_str());
        let compiler = HarnessProfileCompiler::new();
        let spec = synthetic_spec(harness.as_str(), ReasoningPreferenceStrategy::Disabled);
        let (selected, effective) = provider_configuration_ids(b"configuration");
        let model = CompilerModelFacts::new(
            selected,
            effective,
            ProtocolId::new("test.protocol").expect("protocol"),
            RouteId::new("test.route").expect("route"),
            ModelId::new("test-model").expect("model"),
            adapter.clone(),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"]).expect("binding"),
            FeatureSupportFact::Supported,
            FeatureSupportFact::Supported,
            FeatureSupportFact::Supported,
        );
        let inputs = HarnessCompilerInputs::new(
            model,
            tool_facts(&["read_file"]),
            runtime_facts(false),
            Vec::new(),
        )
        .expect("inputs");
        let plan = compiler.compile(&spec, &inputs).expect("plan");
        assert_eq!(plan.profile_id().as_str(), harness.as_str());
        assert_eq!(
            plan.model_facts().model_adapter_profile().as_str(),
            adapter.as_str()
        );
        // Distinct digest domains prove the plan is not a model-adapter binding.
        assert_eq!(
            plan.profile_digest().domain(),
            DigestDomain::HarnessProfileSpec
        );
        assert_eq!(
            plan.plan_digest().domain(),
            DigestDomain::ResolvedHarnessPlan
        );
        assert_eq!(
            plan.model_facts().model_adapter_profile().as_str(),
            plan.profile_id().as_str()
        );
        // Equal display strings do not make the identities interchangeable types.
        let _: &HarnessProfileId = plan.profile_id();
        let _: &ModelAdapterProfileId = plan.model_facts().model_adapter_profile();
    }

    #[test]
    fn tool_override_for_unknown_canonical_tool_fails() {
        let compiler = HarnessProfileCompiler::new();
        let spec = HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plan.tools").expect("id"))
            .base_instructions("tools")
            .tool_overrides(vec![
                ToolExposureOverride::new("missing_tool", "alias", None).expect("override"),
            ])
            .build()
            .expect("spec");
        let error = compiler
            .compile(
                &spec,
                &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
            )
            .expect_err("unknown tool");
        assert!(matches!(error, ProfileError::UnknownCanonicalTool { .. }));
    }

    #[test]
    fn tool_override_is_reconstruction_complete_and_requires_exposure() {
        let compiler = HarnessProfileCompiler::new();
        let override_entry =
            ToolExposureOverride::new("read_file", "read", Some("Read exactly one file".into()))
                .expect("override");
        let exposed = HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plan.exposed-tools").expect("id"))
            .base_instructions("tools")
            .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
            .tool_overrides(vec![override_entry.clone()])
            .build()
            .expect("spec");
        let plan = compiler
            .compile(
                &exposed,
                &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
            )
            .expect("plan");
        assert_eq!(plan.tools().exposed_names(), &["read".to_owned()]);
        assert_eq!(
            plan.tools().overrides(),
            std::slice::from_ref(&override_entry)
        );

        let hidden = HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plan.hidden-tools").expect("id"))
            .base_instructions("tools")
            .tool_exposure_strategy(ToolExposureStrategy::None)
            .tool_overrides(vec![override_entry])
            .build()
            .expect("spec");
        assert_eq!(
            compiler.compile(
                &hidden,
                &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
            ),
            Err(ProfileError::InvalidToolOverride)
        );
    }

    #[test]
    fn recommended_neutral_profile_compiles() {
        let profile = recommended_neutral_coding_profile().expect("recommended");
        let plan = HarnessProfileCompiler::new()
            .compile(
                &profile,
                &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
            )
            .expect("compile");
        assert!(plan.reasoning().enabled());
        assert!(plan.continuation().use_native_continuation());
        assert!(!plan.cache().prefer_stable_prefix());
        assert_eq!(profile.cache_strategy(), CachePreferenceStrategy::Disabled);
        assert_eq!(plan.tools().exposed_names(), &["read_file".to_owned()]);
        assert_eq!(profile.provenance(), HarnessProfileProvenance::Recommended);
    }

    #[test]
    fn preferred_reasoning_effort_override_replaces_profile_default() {
        let profile = recommended_neutral_coding_profile().expect("recommended");
        assert_eq!(
            profile.preferred_reasoning_effort(),
            Some(ReasoningEffort::High)
        );
        let overridden = profile
            .with_preferred_reasoning_effort(ReasoningEffort::XHigh)
            .expect("override");
        assert_eq!(
            overridden.preferred_reasoning_effort(),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            overridden.reasoning_strategy(),
            ReasoningPreferenceStrategy::PreferEffort
        );
        assert_eq!(overridden.base_instructions(), profile.base_instructions());
        let plan = HarnessProfileCompiler::new()
            .compile(
                &overridden,
                &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
            )
            .expect("compile");
        assert_eq!(
            plan.reasoning_policy(),
            ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::XHigh),
                requirement: CapabilityRequirement::Required,
            }
        );
    }
}
