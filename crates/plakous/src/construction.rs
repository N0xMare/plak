//! Construction helpers that compute library-owned facts for ordinary callers.
//!
//! Building a Custom model against the facade means naming a descriptor, an
//! authority, a behavior, and a workspace. It should not also mean restating
//! facts the library already holds. Every helper here derives exactly what the
//! descriptor and the compiled configuration determine, and takes explicitly
//! only what the caller genuinely owns.
//!
//! These helpers materialize complete configuration. None of them consults the
//! environment, discovers a credential, selects a provider default, or falls
//! back to a neighbouring route. An input that disagrees with the descriptor is
//! refused rather than repaired.

use std::sync::Arc;

use plakous_agent::internal::{
    AgentError, AgentErrorKind, HarnessProfileId, HarnessProfileSpecBuilder, ResolvedHarnessPlan,
    ToolExposureStrategy, compile_explicit_runtime_profile,
};
use plakous_model::{
    CompiledProviderConfiguration, ContextContract, CredentialSelectionId, Digest,
    EffectiveProviderConfiguration, FamilyPolicyEnvelope, LanguageModel, ModelAdapterProfileId,
    ModelImplementationIdentity, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
    ProviderConfigurationError, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderSemanticEvidence, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    RuntimeModelAuthority, RuntimeModelAuthorityError, SelectedProviderConfiguration, Timestamp,
};
use plakous_tools::{
    FrozenToolRegistry, PROCESS_TOOL_PROJECTION_REVISION, ProviderModelProjectionIdentityV2,
    WorkspacePolicy,
};

/// The caller-owned half of one Custom provider configuration.
///
/// The route, integration profile, requested model, and model family are read
/// off `descriptor`; the behavior is applied once to both halves of the
/// configuration pipeline. Everything in this struct is a fact the library
/// cannot know: an issuer's revisions, its authority class, which credential
/// selection the caller intends, and the canonical family-policy bytes.
pub struct CustomProviderConfigurationRequest<'a> {
    /// The already resolved Custom descriptor, including its context contract.
    pub descriptor: &'a ResolvedModelDescriptor,
    /// Fresh runtime authority bound to exactly that descriptor.
    pub authority: &'a RuntimeModelAuthority,
    /// The behavior the caller intends, applied once to selection and effect.
    pub behavior: ProviderBehaviorConfiguration,
    /// The caller's revision for the descriptor's own integration profile.
    pub integration_revision: ProviderProfileRevisionId,
    /// The caller's authentication profile.
    pub auth_profile: ModelAdapterProfileId,
    /// The caller's authentication profile revision.
    pub auth_revision: ProviderProfileRevisionId,
    /// The authority class this configuration executes under.
    pub authority_class: ProviderAuthorityClassId,
    /// The exact credential selection, never discovered from the environment.
    pub credential_selection: CredentialSelectionId,
    /// The caller's Custom model implementation identity.
    pub implementation: ModelImplementationIdentity,
    /// Evidence for the provider semantics this configuration assumes.
    pub semantic_evidence: ProviderSemanticEvidence,
    /// Family policy schema revision; must be non-zero.
    pub family_policy_schema_revision: u32,
    /// Family policy compiler revision; must be non-zero.
    pub family_policy_compiler_revision: u32,
    /// Family policy dialect revision; must be non-zero.
    pub family_policy_dialect_revision: u32,
    /// Canonical family-policy bytes the caller seals into the envelope.
    pub family_policy_bytes: Vec<u8>,
}

impl CustomProviderConfigurationRequest<'_> {
    /// Compiles one complete Custom provider configuration.
    ///
    /// The route selection, the family envelope's family, and the stable
    /// authority binding are derived from the descriptor and the authority.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderConfigurationError::InvalidFamilyPolicy`] when the
    /// descriptor names no model family, and otherwise the error the underlying
    /// selection, authority binding, or compilation reports.
    pub fn compile(self) -> Result<CompiledProviderConfiguration, ProviderConfigurationError> {
        let descriptor = self.descriptor;
        let family = descriptor
            .model_family()
            .cloned()
            .ok_or(ProviderConfigurationError::InvalidFamilyPolicy)?;
        let route = ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            self.integration_revision,
            self.auth_profile,
            self.auth_revision,
            self.authority_class,
            self.credential_selection,
            descriptor.requested_model().clone(),
        );
        let selected = SelectedProviderConfiguration::new(route, self.behavior.clone())?;
        let stable = ResolvedProviderAuthorityBinding::new(
            descriptor,
            self.authority,
            self.semantic_evidence,
        )?;
        let family_policy = FamilyPolicyEnvelope::new(
            family,
            self.family_policy_schema_revision,
            self.family_policy_compiler_revision,
            self.family_policy_dialect_revision,
            self.family_policy_bytes,
        )?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            descriptor.clone(),
            self.implementation,
            stable,
            self.behavior,
            family_policy,
        )?;
        CompiledProviderConfiguration::new(selected, effective)
    }
}

/// Binds one Custom descriptor to its context contract and its runtime authority.
///
/// Adding a context contract changes the descriptor's binding digest, so an
/// authority minted before the contract no longer belongs to it. That ordering
/// is easy to get wrong and silently yields a foreign authority later. This
/// helper mints the authority once, hands the caller the authority-class
/// binding its certificates need, and re-binds the authority to the final
/// descriptor, so a caller holds exactly one authority.
///
/// `account` is the caller's non-secret account identity; `credential_authority`
/// identifies the issuer/selection authority, not a credential. `revision` is
/// the caller's authority revision. `checked_at` and `expires_at` bound its
/// freshness. The closure receives the final certificate authority-class digest.
/// Example fixtures may use one digest for both identities; production callers
/// should bind their actual distinct authorities.
///
/// # Errors
///
/// Returns the authority error either binding reports, or the caller's own
/// error from building the contract.
pub fn bind_custom_model_authority<E>(
    descriptor: ResolvedModelDescriptor,
    account: Digest,
    credential_authority: Digest,
    revision: u64,
    checked_at: Timestamp,
    expires_at: Option<Timestamp>,
    context_contract: impl FnOnce(Digest) -> Result<ContextContract, E>,
) -> Result<(ResolvedModelDescriptor, RuntimeModelAuthority), E>
where
    E: From<RuntimeModelAuthorityError>,
{
    let provisional = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential_authority,
        revision,
        checked_at,
        expires_at,
    )
    .map_err(E::from)?;
    let class = provisional.authority_class_binding().map_err(E::from)?;
    let descriptor = descriptor.with_context_contract(context_contract(class)?);
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential_authority,
        revision,
        checked_at,
        expires_at,
    )
    .map_err(E::from)?;
    Ok((descriptor, authority))
}

/// Derives the process-tool projection target a compiled configuration determines.
///
/// Every field of the identity is already held by the configuration; a caller
/// that restates them can only introduce drift.
///
/// # Errors
///
/// Returns invalid input when the configuration's descriptor names no model
/// family, or when the derived tuple is not a portable projection target.
pub fn provider_model_projection_identity(
    configuration: &CompiledProviderConfiguration,
) -> Result<ProviderModelProjectionIdentityV2, AgentError> {
    let effective = configuration.effective();
    let descriptor = effective.descriptor();
    let family = descriptor.model_family().ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "a projection target requires the descriptor's model family",
        )
    })?;
    ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        family.as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("projection target is not portable: {error}"),
        )
    })
}

/// Compiles the harness plan a local-composition deployment needs.
///
/// A composition deployment exposes its nested catalog through the evaluator,
/// so the provider-visible exposure strategy is [`ToolExposureStrategy::None`].
/// Naming that, the registry, the workspace, and the configuration once is all
/// this plan requires. The plan is compiled for the local runner
/// ([`ExecutionBackendDescriptor::local_runner`](crate::tools::ExecutionBackendDescriptor::local_runner));
/// a deployment on another runner compiles through
/// [`compile_explicit_runtime_profile`] with that runner's descriptor.
///
/// # Errors
///
/// Returns the error the profile specification or the explicit profile
/// compilation reports.
pub fn compile_local_composition_plan(
    profile: HarnessProfileId,
    model: &Arc<dyn LanguageModel>,
    configuration: &CompiledProviderConfiguration,
    tools: &FrozenToolRegistry,
    workspace: &WorkspacePolicy,
) -> Result<ResolvedHarnessPlan, AgentError> {
    let spec = HarnessProfileSpecBuilder::new()
        .id(profile)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .build()
        .map_err(|error| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                format!("local-composition harness profile is invalid: {error}"),
            )
        })?;
    compile_explicit_runtime_profile(
        &spec,
        model.as_ref(),
        configuration,
        tools,
        workspace,
        &plakous_tools::ExecutionBackendDescriptor::local_runner(),
        Vec::new(),
    )
}

#[cfg(test)]
mod tests;
