use super::{
    CachePolicy, ConfiguredRuntimeAuthority, Deserialize, Deserializer, Digest, DigestDomain,
    EffectiveProviderConfiguration, EffectiveProviderConfigurationId, NonZeroU32,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProviderBehaviorConfiguration,
    ProviderConfigurationError, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, ResolvedModelDescriptor,
    ResponseStateDisposition, RuntimeModelAuthority, SELECTED_PROVIDER_CONFIGURATION_REVISION,
    SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS, SelectedProviderConfiguration,
    SelectedProviderConfigurationId, Serialize, Timestamp, ToolChoice, ToolPresentationMode,
    configuration_digest, effective_identity, validate_resolution,
};

/// Complete selected and effective provider configuration authority.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompiledProviderConfiguration {
    selected: SelectedProviderConfiguration,
    effective: EffectiveProviderConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompiledProviderConfigurationWire {
    selected: SelectedProviderConfiguration,
    effective: EffectiveProviderConfiguration,
}

impl<'de> Deserialize<'de> for CompiledProviderConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CompiledProviderConfigurationWire::deserialize(deserializer)?;
        Self::new(wire.selected, wire.effective).map_err(serde::de::Error::custom)
    }
}

impl CompiledProviderConfiguration {
    /// Creates and cross-validates one compiled configuration aggregate.
    ///
    /// # Errors
    ///
    /// Returns an error when the effective value belongs to another selection.
    pub fn new(
        selected: SelectedProviderConfiguration,
        effective: EffectiveProviderConfiguration,
    ) -> Result<Self, ProviderConfigurationError> {
        if effective.selected_identity != selected.identity {
            return Err(ProviderConfigurationError::ForeignSelection);
        }
        if effective.behavior != selected.behavior {
            return Err(ProviderConfigurationError::UnsupportedBehavior);
        }
        if effective.descriptor.model_family() != Some(effective.family_policy.family()) {
            return Err(ProviderConfigurationError::ForeignTarget);
        }
        validate_resolution(
            &selected,
            &effective.descriptor,
            &effective.model_implementation,
            &effective.authority,
            &effective.behavior,
        )?;
        Ok(Self {
            selected,
            effective,
        })
    }

    /// Recompiles this configuration as the driver-owned no-tools compact operation.
    ///
    /// Live Compact must not use this path. Product Compact keeps the
    /// coding runtime and a trailing-user instruction. This helper remains for
    /// offline compiler tests of the retired tools-off / cache-off shape.
    ///
    /// Route, descriptor, authority, family policy, processing tier, storage,
    /// retention, and context-evidence class stay exact. Tools, reasoning,
    /// cache, and replay are disabled; the output bound is
    /// [`SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS`].
    ///
    /// # Errors
    ///
    /// Returns an error when the compaction behavior cannot be sealed against
    /// this source configuration.
    pub fn semantic_compaction(&self) -> Result<Self, ProviderConfigurationError> {
        let source = self.effective.behavior();
        let behavior = ProviderBehaviorConfiguration {
            processing_tier: source.processing_tier.clone(),
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::Disabled,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS),
            storage: source.storage,
            retention: source.retention.clone(),
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: source.context_evidence_requirement,
            local_durability: source.local_durability,
        };
        let selected =
            SelectedProviderConfiguration::new(self.selected.route.clone(), behavior.clone())?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            self.effective.descriptor.clone(),
            self.effective.model_implementation.clone(),
            self.effective.authority.clone(),
            behavior,
            self.effective.family_policy.clone(),
        )?;
        Self::new(selected, effective)
    }

    /// Returns the complete selected configuration.
    #[must_use]
    pub const fn selected(&self) -> &SelectedProviderConfiguration {
        &self.selected
    }
    /// Returns the complete effective configuration.
    #[must_use]
    pub const fn effective(&self) -> &EffectiveProviderConfiguration {
        &self.effective
    }
    /// Returns the selected configuration identity.
    #[must_use]
    pub const fn selected_identity(&self) -> SelectedProviderConfigurationId {
        self.selected.identity
    }
    /// The identity pair a persisted record must be compared against on reopen.
    ///
    /// The runtime manifest is the one gate that asks this, and it asks it of
    /// the persisted prefix and the persisted harness plan together. Every
    /// other reopen comparison is exact, because a reopened session is compiled
    /// with the bound it was created under and so carries the same identity it
    /// persisted. What remains for this is a session created under a
    /// product-chosen figure on a route that has since started deriving one. A
    /// route that
    /// derives its bound from its own attestation moves that figure when the
    /// provider's catalog moves, and the figure is identity-bearing, so the
    /// persisted pair is admitted when it is the pair this very configuration
    /// would carry under the bound the session was created under. Everything
    /// else still has to match exactly. A route whose bound the product chose
    /// keeps the exact comparison, so a changed bound there is still drift.
    ///
    /// A session created before the route derived its bound persisted the
    /// figure as [`OutputBoundDisposition::Enforced`]; one created after
    /// persisted it as [`OutputBoundDisposition::DerivedFromRoute`]. Both name
    /// the same session on the same route, so both spellings are admitted.
    ///
    /// Returns this configuration's own pair when nothing is substituted, which
    /// is what an exact comparison then refuses.
    ///
    /// # Errors
    ///
    /// Returns an identity error when a substituted configuration cannot be
    /// digested.
    pub fn reopen_identities(
        &self,
        persisted_bound: Option<NonZeroU32>,
        persisted: (
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        ),
    ) -> Result<
        (
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        ),
        ProviderConfigurationError,
    > {
        let live = (self.selected_identity(), self.effective_identity());
        if !self
            .effective()
            .behavior()
            .output_bound
            .is_derived_from_route()
        {
            return Ok(live);
        }
        let candidates = match persisted_bound {
            None => vec![OutputBoundDisposition::NoRequestBound],
            Some(bound) => vec![
                OutputBoundDisposition::DerivedFromRoute(bound),
                OutputBoundDisposition::Enforced(bound),
            ],
        };
        for candidate in candidates {
            let substituted = self.identities_with_output_bound(candidate)?;
            if substituted == persisted {
                return Ok(substituted);
            }
        }
        Ok(live)
    }

    /// The identity pair this configuration would carry with a different
    /// product-derived output bound.
    ///
    /// A bound the product derives from a route's own attestation moves when
    /// the provider's catalog moves, and it is identity-bearing. Reopen uses
    /// this to ask the only question that matters: with the bound the session
    /// was created under, is everything else — route, model, authority,
    /// reasoning, tools, policy — still exactly the same? If it is, the session
    /// is the same session and the new bound is upstream news, not drift.
    ///
    /// The disposition is supplied rather than assumed, because a session
    /// created before the route derived its bound persisted it as
    /// [`OutputBoundDisposition::Enforced`], and that session has to reopen
    /// too.
    ///
    /// # Errors
    ///
    /// Returns an identity error when the substituted configuration cannot be
    /// digested.
    pub fn identities_with_output_bound(
        &self,
        output_bound: OutputBoundDisposition,
    ) -> Result<
        (
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        ),
        ProviderConfigurationError,
    > {
        let mut behavior = self.selected.behavior.clone();
        behavior.output_bound = output_bound;
        let bytes = serde_json::to_vec(&(
            &self.selected.route,
            &behavior,
            SELECTED_PROVIDER_CONFIGURATION_REVISION,
        ))
        .map_err(|_| ProviderConfigurationError::Identity)?;
        let selected = SelectedProviderConfigurationId::new(configuration_digest(
            DigestDomain::SelectedProviderConfiguration,
            5,
            b"plakous/selected-provider-configuration-v5",
            &bytes,
        )?)?;
        let effective = effective_identity(
            selected,
            &self.effective.descriptor,
            &self.effective.model_implementation,
            &self.effective.authority,
            &behavior,
            &self.effective.family_policy,
        )?;
        Ok((selected, effective))
    }

    /// Returns the effective configuration identity.
    #[must_use]
    pub const fn effective_identity(&self) -> EffectiveProviderConfigurationId {
        self.effective.identity
    }

    /// Seals fresh adapter authority to this exact compiled configuration.
    ///
    /// # Errors
    ///
    /// Rejects descriptor, semantic/catalog, account, credential-authority,
    /// freshness, or effective-configuration disagreement.
    pub fn seal_runtime_authority(
        &self,
        descriptor: &ResolvedModelDescriptor,
        authority: RuntimeModelAuthority,
        checked_at: Timestamp,
    ) -> Result<ConfiguredRuntimeAuthority, ProviderConfigurationError> {
        authority
            .validate(descriptor, checked_at)
            .map_err(|_| ProviderConfigurationError::ForeignAuthority)?;
        let effective = self.effective();
        let stable = effective.authority();
        if descriptor != effective.descriptor()
            || descriptor
                .semantic_binding_digest()
                .map_err(|_| ProviderConfigurationError::Identity)?
                != stable.semantic_descriptor_binding()
            || authority.account_binding() != stable.account_binding()
            || authority.credential_authority_binding() != stable.credential_authority_binding()
        {
            return Err(ProviderConfigurationError::ForeignAuthority);
        }
        let binding = Digest::sha256(
            DigestDomain::ModelBinding,
            6,
            &[
                b"plakous-model/configured-runtime-authority-v1",
                &authority
                    .execution_authority_binding()
                    .map_err(|_| ProviderConfigurationError::Identity)?
                    .identity_bytes(),
                &self.effective_identity().digest().identity_bytes(),
                &stable.semantic_descriptor_binding().identity_bytes(),
            ],
        )
        .map_err(|_| ProviderConfigurationError::Identity)?;
        Ok(ConfiguredRuntimeAuthority {
            effective_configuration: self.effective_identity(),
            runtime: authority,
            binding,
        })
    }
}
