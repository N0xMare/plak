use super::{
    CapabilityRequirement, CompilerModelFacts, Digest, EffectiveProviderConfigurationId,
    HarnessCompilerInputs, HarnessProfileId, HarnessProfileSpec, InstructionBlock,
    InstructionCompositionStrategy, InstructionSource, ProfileError, ReasoningEffort,
    ReasoningPolicy, ResolvedCachePlan, ResolvedContextSourceFact, ResolvedContinuationPlan,
    ResolvedHarnessPlan, ResolvedInstructionPlan, ResolvedReasoningPlan, ResolvedToolExposurePlan,
    SafeDiagnostic, SelectedProviderConfigurationId, ToolExposureOverride, ToolExposureStrategy,
};

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

impl ResolvedContinuationPlan {
    /// Returns whether native continuation was selected.
    #[must_use]
    pub const fn use_native_continuation(self) -> bool {
        self.use_native_continuation
    }
}

impl ResolvedCachePlan {
    /// Returns whether stable-prefix caching was selected.
    #[must_use]
    pub const fn prefer_stable_prefix(self) -> bool {
        self.prefer_stable_prefix
    }
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
