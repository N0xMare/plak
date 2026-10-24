//! Product extension evidence: limits and per-language validation.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

/// The source-language semantics owned by one exact local-composition profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LocalCompositionLanguage {
    /// JavaScript semantics. This does not authorize Python or source translation.
    JavaScript,
    /// Python semantics for the admitted Monty-backed Python PTC profile.
    Python,
}

/// Bounded product-owned limits bound by extension evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductExtensionLimits {
    /// Maximum UTF-8 source bytes accepted before evaluator dispatch.
    pub max_source_bytes: u64,
    /// Maximum time one evaluator yield may occupy.
    pub max_yield_time_ms: u64,
    /// Maximum result budget exposed by the product profile.
    pub max_output_tokens: u64,
}

/// Independent receipt for one Plak-owned local-composition extension.
///
/// This receipt binds only the product extension layered over an already
/// admitted Standard or Custom target. It is deliberately not context or
/// provider-native evidence and cannot replace the target's own evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductExtensionEvidence {
    target: IntegrationClass,
    profile: Digest,
    artifact: Digest,
    catalog: Digest,
    presentation: Digest,
    limits: ProductExtensionLimits,
    host: Digest,
    engine_contract: Digest,
    containment: Digest,
    product_issuer: Digest,
    qualification_receipt: Digest,
    selected_configuration: SelectedProviderConfigurationId,
    effective_configuration: EffectiveProviderConfigurationId,
    receipt: Digest,
}

impl ProductExtensionEvidence {
    /// Derives exact product-extension evidence for `PythonPtcV1`.
    ///
    /// # Errors
    ///
    /// Rejects a foreign profile, target, artifact, or provider configuration.
    #[cfg(feature = "composition-python")]
    pub fn python_ptc_v1(
        profile: &LocalCompositionProfile,
        artifact: &LocalCompositionEngineArtifact,
        product_issuer: Digest,
        qualification_receipt: Digest,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<Self, AgentError> {
        let LocalCompositionProfileKind::PythonPtcV1(descriptor) = &profile.kind else {
            return Err(invalid("Python extension evidence requires PythonPtcV1"));
        };
        let target = descriptor.outer_projection().target().integration_class();
        if configuration.effective().descriptor().integration_class() != target {
            return Err(invalid(
                "Python extension evidence disagrees with its admitted target",
            ));
        }
        let limits = python_extension_limits();
        extension_evidence(
            target,
            profile.identity(),
            artifact,
            descriptor.catalog_identity(),
            descriptor.presentation_identity(),
            limits,
            product_issuer,
            qualification_receipt,
            configuration,
        )
    }

    /// Derives one self-attested product-extension receipt from the complete
    /// product JavaScript attachment and an external qualification candidate.
    ///
    /// # Errors
    ///
    /// Rejects a native profile, any non-exact V1 limit tuple, an empty issuer
    /// or qualification identity, or a target foreign to the compiled configuration.
    /// This constructor makes no provider-native or context-evidence claim.
    pub fn product_javascript_v1(
        profile: &LocalCompositionProfile,
        artifact: &LocalCompositionEngineArtifact,
        limits: ProductExtensionLimits,
        product_issuer: Digest,
        qualification_receipt: Digest,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<Self, AgentError> {
        let LocalCompositionProfileKind::ProductJavaScriptV1(descriptor) = &profile.kind else {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "product extension evidence requires a product-owned JavaScript profile",
            ));
        };
        let target = descriptor.outer_projection().target().integration_class();
        if !product_javascript_limits_are_exact(limits)
            || configuration.effective().descriptor().integration_class() != target
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "product extension evidence disagrees with its exact admitted target",
            ));
        }
        let profile = profile.identity();
        let artifact_identity = artifact.identity();
        let catalog = descriptor.nested_projection().digest();
        let presentation = descriptor.outer_projection().digest();
        let host = artifact.host_identity();
        let engine_contract = artifact.engine_contract_identity();
        let containment = artifact.containment().identity();
        let selected_configuration = configuration.selected_identity();
        let effective_configuration = configuration.effective_identity();
        let receipt = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.product-extension-evidence.v1",
                &[match target {
                    IntegrationClass::Standard => 1,
                    IntegrationClass::Custom => 2,
                    _ => {
                        return Err(AgentError::new(
                            AgentErrorKind::InvalidInput,
                            "unknown product extension target class",
                        ));
                    }
                }],
                &profile.identity_bytes(),
                &artifact_identity.identity_bytes(),
                &catalog.identity_bytes(),
                &presentation.identity_bytes(),
                &limits.max_source_bytes.to_be_bytes(),
                &limits.max_yield_time_ms.to_be_bytes(),
                &limits.max_output_tokens.to_be_bytes(),
                &host.identity_bytes(),
                &engine_contract.identity_bytes(),
                &containment.identity_bytes(),
                &product_issuer.identity_bytes(),
                &qualification_receipt.identity_bytes(),
                &selected_configuration.digest().identity_bytes(),
                &effective_configuration.digest().identity_bytes(),
            ],
        )
        .map_err(|error| AgentError::new(AgentErrorKind::Identity, error.to_string()))?;
        Ok(Self {
            target,
            profile,
            artifact: artifact_identity,
            catalog,
            presentation,
            limits,
            host,
            engine_contract,
            containment,
            product_issuer,
            qualification_receipt,
            selected_configuration,
            effective_configuration,
            receipt,
        })
    }

    /// Returns the product issuer and qualification-candidate receipt identities.
    #[must_use]
    pub const fn product_authority_identities(&self) -> (Digest, Digest) {
        (self.product_issuer, self.qualification_receipt)
    }

    /// Returns the exact engine and truthful containment identities.
    #[must_use]
    pub const fn engine_identities(&self) -> (Digest, Digest) {
        (self.engine_contract, self.containment)
    }

    /// Product-extension evidence is explicitly product-owned and self-attested.
    #[must_use]
    pub const fn is_product_self_attested(&self) -> bool {
        true
    }

    /// Returns the exact product-extension receipt identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.receipt
    }

    /// Returns the admitted integration class beneath this extension.
    #[must_use]
    pub const fn target(&self) -> IntegrationClass {
        self.target
    }

    /// Returns the exact profile, artifact, catalog, presentation, limits, and host tuple.
    #[must_use]
    pub const fn extension_facts(
        &self,
    ) -> (
        Digest,
        Digest,
        Digest,
        Digest,
        ProductExtensionLimits,
        Digest,
    ) {
        (
            self.profile,
            self.artifact,
            self.catalog,
            self.presentation,
            self.limits,
            self.host,
        )
    }

    /// Returns the selected/effective configuration identities bound by the receipt.
    #[must_use]
    pub const fn configuration_identities(
        &self,
    ) -> (
        SelectedProviderConfigurationId,
        EffectiveProviderConfigurationId,
    ) {
        (self.selected_configuration, self.effective_configuration)
    }

    /// Product-extension receipts never claim provider-native support.
    #[must_use]
    pub const fn makes_provider_native_claim(&self) -> bool {
        false
    }
}

pub(super) fn validate_product_evidence(
    profile: &LocalCompositionProfile,
    artifact: &LocalCompositionEngineArtifact,
    descriptor: &ProductJavaScriptCompositionDescriptorV1,
    evidence: &ProductExtensionEvidence,
) -> Result<(), AgentError> {
    let (profile_fact, artifact_fact, catalog, presentation, _, host) = evidence.extension_facts();
    let (_, _, _, _, limits, _) = evidence.extension_facts();
    let (engine_contract, containment) = evidence.engine_identities();
    if evidence.target() != descriptor.outer_projection().target().integration_class()
        || profile_fact != profile.identity()
        || artifact_fact != artifact.identity()
        || catalog != descriptor.nested_projection().digest()
        || presentation != descriptor.outer_projection().digest()
        || host != artifact.host_identity()
        || engine_contract != artifact.engine_contract_identity()
        || containment != artifact.containment().identity()
        || !product_javascript_limits_are_exact(limits)
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "product extension evidence disagrees with the exact profile attachment",
        ));
    }
    Ok(())
}

pub(super) fn product_javascript_limits_are_exact(limits: ProductExtensionLimits) -> bool {
    u64::try_from(native::MAX_SOURCE_BYTES).is_ok_and(|max_source_bytes| {
        limits.max_source_bytes == max_source_bytes
            && limits.max_yield_time_ms == PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS
            && limits.max_output_tokens == PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS
    })
}

#[cfg(feature = "composition-python")]
pub(super) const fn python_extension_limits() -> ProductExtensionLimits {
    ProductExtensionLimits {
        max_source_bytes: PythonPtcLimitsV1::EXACT.source_bytes,
        max_yield_time_ms: PYTHON_DEFAULT_YIELD_TIME_MS,
        max_output_tokens: PythonPtcLimitsV1::EXACT.final_result_bytes,
    }
}

#[cfg(feature = "composition-python")]
#[allow(
    clippy::too_many_arguments,
    reason = "the evidence receipt deliberately binds one closed reconstruction tuple"
)]
fn extension_evidence(
    target: IntegrationClass,
    profile: Digest,
    artifact: &LocalCompositionEngineArtifact,
    catalog: Digest,
    presentation: Digest,
    limits: ProductExtensionLimits,
    product_issuer: Digest,
    qualification_receipt: Digest,
    configuration: &CompiledProviderConfiguration,
) -> Result<ProductExtensionEvidence, AgentError> {
    let artifact_identity = artifact.identity();
    let host = artifact.host_identity();
    let engine_contract = artifact.engine_contract_identity();
    let containment = artifact.containment().identity();
    let selected_configuration = configuration.selected_identity();
    let effective_configuration = configuration.effective_identity();
    let receipt = Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[
            b"plakous.product-extension-evidence.v1",
            &[match target {
                IntegrationClass::Standard => 1,
                IntegrationClass::Custom => 2,
                _ => return Err(invalid("unknown product extension target class")),
            }],
            &profile.identity_bytes(),
            &artifact_identity.identity_bytes(),
            &catalog.identity_bytes(),
            &presentation.identity_bytes(),
            &limits.max_source_bytes.to_be_bytes(),
            &limits.max_yield_time_ms.to_be_bytes(),
            &limits.max_output_tokens.to_be_bytes(),
            &host.identity_bytes(),
            &engine_contract.identity_bytes(),
            &containment.identity_bytes(),
            &product_issuer.identity_bytes(),
            &qualification_receipt.identity_bytes(),
            &selected_configuration.digest().identity_bytes(),
            &effective_configuration.digest().identity_bytes(),
        ],
    )
    .map_err(identity_error)?;
    Ok(ProductExtensionEvidence {
        target,
        profile,
        artifact: artifact_identity,
        catalog,
        presentation,
        limits,
        host,
        engine_contract,
        containment,
        product_issuer,
        qualification_receipt,
        selected_configuration,
        effective_configuration,
        receipt,
    })
}

#[cfg(feature = "composition-python")]
pub(super) fn validate_python_evidence(
    profile: &LocalCompositionProfile,
    artifact: &LocalCompositionEngineArtifact,
    descriptor: &PythonPtcDescriptorV1,
    evidence: &ProductExtensionEvidence,
) -> Result<(), AgentError> {
    let (profile_fact, artifact_fact, catalog, presentation, limits, host) =
        evidence.extension_facts();
    let (engine_contract, containment) = evidence.engine_identities();
    if evidence.target() != descriptor.outer_projection().target().integration_class()
        || profile_fact != profile.identity()
        || artifact_fact != artifact.identity()
        || catalog != descriptor.catalog_identity()
        || presentation != descriptor.presentation_identity()
        || limits != python_extension_limits()
        || host != artifact.host_identity()
        || engine_contract != artifact.engine_contract_identity()
        || containment != artifact.containment().identity()
    {
        return Err(invalid(
            "Python extension evidence disagrees with the exact profile attachment",
        ));
    }
    Ok(())
}
