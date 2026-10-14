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
/// this plan requires.
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
        Vec::new(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use plakous_model::{
        CachePolicy, Capability, CapabilityConstraints, CapabilitySet, CapacityCertificate,
        CapacityRule, CompiledProviderConfiguration, ConfiguredTool, ContextAdmissionRequirement,
        ContextContract, ContextEvidenceSource, CountCertificateKind, CredentialSelectionId,
        Digest, DigestDomain, EffectiveProviderConfiguration, EvidenceFreshness, FactProvenance,
        FactSource, FamilyPolicyEnvelope, FeatureSupport, LocalDurabilityDisposition,
        MeasurementCertificate, ModelAdapterProfileId, ModelFamilyId, ModelId,
        ModelImplementationIdentity, NamespaceId, OutputBoundDisposition, OutputPolicy,
        ParallelToolDisposition, ProcessingTierDisposition, ProtocolId, ProviderAuthorityClassId,
        ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
        ProviderSemanticEvidence, ProviderStorageDisposition, ReasoningEffortDisposition,
        ReasoningIncludeDisposition, ReasoningSummaryDisposition, ReplayDisposition,
        ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, ResponseStateDisposition,
        RetentionDisposition, RouteId, RuntimeModelAuthority, SelectedProviderConfiguration,
        Support, Timestamp, ToolChoice, ToolPresentationMode,
    };

    use super::{CustomProviderConfigurationRequest, provider_model_projection_identity};

    type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

    const PROTOCOL: &str = "consumer-protocol";
    const ROUTE: &str = "consumer-route";
    const MODEL: &str = "consumer-model";
    const FAMILY: &str = "consumer-family";
    const PROFILE: &str = "consumer-profile";
    const SEMANTICS: &[u8] = b"consumer-semantics-v1";
    const FAMILY_POLICY: &[u8] = b"consumer-family-policy-v1";

    /// Builds the same Custom descriptor and authority the fixture builds.
    fn descriptor_and_authority() -> Fallible<(ResolvedModelDescriptor, RuntimeModelAuthority)> {
        let time = Timestamp::new(1, 0)?;
        let features = [Capability::Tools, Capability::Streaming]
            .into_iter()
            .map(|capability| {
                (
                    capability,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        FactProvenance::new(FactSource::TestFixture, time, None),
                    ),
                )
            })
            .collect();
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new(PROTOCOL)?,
            RouteId::new(ROUTE)?,
            ModelId::new(MODEL)?,
            None,
            Some(ModelFamilyId::new(FAMILY)?),
            ModelAdapterProfileId::new(PROFILE)?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"construction-helper"])?;
        let authority = RuntimeModelAuthority::new(&descriptor, binding, binding, 1, time, None)?;
        let class = authority.authority_class_binding()?;
        let freshness = EvidenceFreshness::new(time, None)?;
        let descriptor = descriptor.with_context_contract(ContextContract::new(
            ProtocolId::new(PROTOCOL)?,
            "consumer-accounting-v1",
            "consumer-projection-v1",
            "consumer-reservation-v1",
            CapacityCertificate::custom_envelope(
                "consumer-capacity",
                "v1",
                CapacityRule::ProviderReservedInput {
                    reported_context_window: None,
                    maximum_projected_input: 1_000_000,
                    auto_compact_input: None,
                },
                class,
                freshness,
                binding,
                "v1",
            )?,
            vec![MeasurementCertificate::new(
                "consumer-counter",
                "v1",
                "wire-bytes",
                "v1",
                ContextEvidenceSource::CallerDeclaration,
                CountCertificateKind::CustomEnvelopeUpperBound {
                    declaration: binding,
                },
                Some(class),
                freshness,
            )?],
        )?);
        let authority = RuntimeModelAuthority::new(&descriptor, binding, binding, 1, time, None)?;
        Ok((descriptor, authority))
    }

    /// Returns the behavior both construction paths share.
    fn behavior(descriptor: &ResolvedModelDescriptor) -> Fallible<ProviderBehaviorConfiguration> {
        Ok(ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::Disabled,
            cache: CachePolicy::Disabled,
            tools: Vec::<ConfiguredTool>::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::NoRequestBound,
            storage: ProviderStorageDisposition::OmittedRouteContract,
            retention: RetentionDisposition::RouteContract {
                profile: descriptor.profile().clone(),
                revision: ProviderProfileRevisionId::new("consumer-retention-v1")?,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::DurableRequired,
        })
    }

    /// Returns the caller's model implementation identity.
    fn implementation() -> Fallible<ModelImplementationIdentity> {
        Ok(ModelImplementationIdentity::custom(
            NamespaceId::new("consumer.model")?,
            1,
            NamespaceId::new("consumer.fixture")?,
        )?)
    }

    /// Compiles the configuration the long way, exactly as a caller did before.
    fn hand_built(
        descriptor: &ResolvedModelDescriptor,
        authority: &RuntimeModelAuthority,
    ) -> Fallible<CompiledProviderConfiguration> {
        let behavior = behavior(descriptor)?;
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.profile().clone(),
                ProviderProfileRevisionId::new("consumer-route-v1")?,
                ModelAdapterProfileId::new("consumer.auth")?,
                ProviderProfileRevisionId::new("consumer-auth-v1")?,
                ProviderAuthorityClassId::new("consumer-scripted")?,
                CredentialSelectionId::new("consumer-credential")?,
                descriptor.requested_model().clone(),
            ),
            behavior.clone(),
        )?;
        let stable = ResolvedProviderAuthorityBinding::new(
            descriptor,
            authority,
            ProviderSemanticEvidence::new(1, SEMANTICS.to_vec())?,
        )?;
        let family = FamilyPolicyEnvelope::new(
            descriptor.model_family().cloned().ok_or("missing family")?,
            1,
            1,
            1,
            FAMILY_POLICY.to_vec(),
        )?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            descriptor.clone(),
            implementation()?,
            stable,
            behavior,
            family,
        )?;
        Ok(CompiledProviderConfiguration::new(selected, effective)?)
    }

    /// Compiles the same configuration through the construction helper.
    fn helper_built(
        descriptor: &ResolvedModelDescriptor,
        authority: &RuntimeModelAuthority,
    ) -> Fallible<CompiledProviderConfiguration> {
        Ok(CustomProviderConfigurationRequest {
            descriptor,
            authority,
            behavior: behavior(descriptor)?,
            integration_revision: ProviderProfileRevisionId::new("consumer-route-v1")?,
            auth_profile: ModelAdapterProfileId::new("consumer.auth")?,
            auth_revision: ProviderProfileRevisionId::new("consumer-auth-v1")?,
            authority_class: ProviderAuthorityClassId::new("consumer-scripted")?,
            credential_selection: CredentialSelectionId::new("consumer-credential")?,
            implementation: implementation()?,
            semantic_evidence: ProviderSemanticEvidence::new(1, SEMANTICS.to_vec())?,
            family_policy_schema_revision: 1,
            family_policy_compiler_revision: 1,
            family_policy_dialect_revision: 1,
            family_policy_bytes: FAMILY_POLICY.to_vec(),
        }
        .compile()?)
    }

    #[test]
    fn the_helper_compiles_the_configuration_the_long_form_compiles() -> Fallible<()> {
        let (descriptor, authority) = descriptor_and_authority()?;
        let hand = hand_built(&descriptor, &authority)?;
        let helper = helper_built(&descriptor, &authority)?;
        assert_eq!(hand.selected_identity(), helper.selected_identity());
        assert_eq!(hand.effective_identity(), helper.effective_identity());
        assert_eq!(
            hand.effective().effective_model(),
            helper.effective().effective_model()
        );
        assert_eq!(
            hand.effective().descriptor().binding_digest()?,
            helper.effective().descriptor().binding_digest()?
        );
        Ok(())
    }

    #[test]
    fn the_helper_derives_the_projection_identity_a_caller_restated() -> Fallible<()> {
        let (descriptor, authority) = descriptor_and_authority()?;
        let configuration = hand_built(&descriptor, &authority)?;
        let effective = configuration.effective();
        let effective_descriptor = effective.descriptor();
        let restated = plakous_tools::ProviderModelProjectionIdentityV2::new(
            effective_descriptor.integration_class(),
            effective_descriptor.protocol().as_str(),
            effective_descriptor.route().as_str(),
            effective_descriptor.profile().as_str(),
            effective_descriptor
                .model_family()
                .ok_or("missing family")?
                .as_str(),
            effective_descriptor.requested_model().as_str(),
            effective.effective_model().as_str(),
            configuration.effective_identity(),
            plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        assert_eq!(
            restated,
            provider_model_projection_identity(&configuration)?
        );
        Ok(())
    }
}
