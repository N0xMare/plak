use super::{
    Arc, CachePreferenceStrategy, ContextSourceSelector, ContinuationPreferenceStrategy, Digest,
    DigestDomain, HARNESS_PROFILE_SCHEMA_VERSION, HarnessProfileId, HarnessProfileProvenance,
    HarnessProfileSpec, HarnessProfileSpecBuilder, InstructionCompositionStrategy,
    MAX_INSTRUCTION_BYTES, MAX_SOURCE_SELECTORS, MAX_TEMPLATE_BYTES, MAX_TOOL_EXPOSURE_OVERRIDES,
    ProfileError, ProfileSemanticVersion, ReasoningEffort, ReasoningPreferenceStrategy,
    ResolvedContextSourceFact, SPEC_DIGEST_VERSION, ToolExposureOverride, ToolExposureStrategy,
    VerifiedContextSourceRef, context_source_digest, fmt, validate_identifier,
    validate_source_digest, validate_source_metadata, validate_text_bound,
};

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

    pub(super) fn validate(&self) -> Result<(), ProfileError> {
        if !validate_identifier(&self.reference_id) || !validate_source_metadata(&self.version) {
            return Err(ProfileError::InvalidContextSourceReference);
        }
        validate_source_digest(self.content_digest)
    }
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

    pub(super) fn validate(&self) -> Result<(), ProfileError> {
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
