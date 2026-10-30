//! Product extension evidence for exact cell attachments.

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
    /// Derives a self-attested receipt for an exact product-owned cell profile.
    ///
    /// # Errors
    ///
    /// Rejects a native profile, a foreign engine, or a provider configuration
    /// from another target. Exact limits come from the selected profile.
    /// This makes no provider-native or successful live-qualification claim.
    pub fn cell(
        profile: &LocalCompositionProfile,
        artifact: &LocalCompositionEngineArtifact,
        product_issuer: Digest,
        qualification_receipt: Digest,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<Self, AgentError> {
        let facts = extension_facts(profile)?;
        validate_engine_contract_binding(
            profile.engine_contract_identity(),
            artifact.engine_contract_identity(),
        )?;
        let target = facts.target;
        if configuration.effective().descriptor().integration_class() != target {
            return Err(invalid(
                "cell extension evidence disagrees with its admitted target",
            ));
        }
        extension_evidence(
            profile.identity(),
            artifact,
            facts,
            product_issuer,
            qualification_receipt,
            configuration,
        )
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
    evidence: &ProductExtensionEvidence,
) -> Result<(), AgentError> {
    let facts = extension_facts(profile)?;
    let (profile_fact, artifact_fact, catalog, presentation, limits, host) =
        evidence.extension_facts();
    let (engine_contract, containment) = evidence.engine_identities();
    if evidence.target() != facts.target
        || profile_fact != profile.identity()
        || artifact_fact != artifact.identity()
        || catalog != facts.catalog
        || presentation != facts.presentation
        || host != artifact.host_identity()
        || engine_contract != artifact.engine_contract_identity()
        || containment != artifact.containment().identity()
        || limits != facts.limits
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "product extension evidence disagrees with the exact profile attachment",
        ));
    }
    Ok(())
}

pub(super) fn javascript_extension_limits() -> ProductExtensionLimits {
    ProductExtensionLimits {
        max_source_bytes: native::MAX_SOURCE_BYTES as u64,
        max_yield_time_ms: PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS,
        max_output_tokens: PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS,
    }
}

pub(super) const fn python_extension_limits() -> ProductExtensionLimits {
    ProductExtensionLimits {
        max_source_bytes: PythonPtcLimitsV1::EXACT.source_bytes,
        max_yield_time_ms: PYTHON_DEFAULT_YIELD_TIME_MS,
        max_output_tokens: PythonPtcLimitsV1::EXACT.final_result_bytes,
    }
}

fn extension_evidence(
    profile: Digest,
    artifact: &LocalCompositionEngineArtifact,
    facts: ExtensionFacts,
    product_issuer: Digest,
    qualification_receipt: Digest,
    configuration: &CompiledProviderConfiguration,
) -> Result<ProductExtensionEvidence, AgentError> {
    let ExtensionFacts {
        target,
        catalog,
        presentation,
        limits,
    } = facts;
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

#[derive(Clone, Copy)]
struct ExtensionFacts {
    target: IntegrationClass,
    catalog: Digest,
    presentation: Digest,
    limits: ProductExtensionLimits,
}

fn extension_facts(profile: &LocalCompositionProfile) -> Result<ExtensionFacts, AgentError> {
    let limits = match &profile.descriptor {
        plakous_compose::CellDescriptor::ProductJavaScriptV1(_) => javascript_extension_limits(),
        plakous_compose::CellDescriptor::PythonPtcV1(_) => python_extension_limits(),
        plakous_compose::CellDescriptor::PythonHarnessSdkV1(_) => ProductExtensionLimits {
            max_source_bytes: u64::from(
                plakous_compose::PythonHarnessSdkDescriptorV1::SOURCE_BYTES,
            ),
            max_yield_time_ms: plakous_compose::PythonHarnessSdkDescriptorV1::FOREGROUND_MS,
            max_output_tokens: plakous_compose::PythonHarnessSdkDescriptorV1::OUTPUT_BYTES as u64,
        },
        plakous_compose::CellDescriptor::CodexNativeLocalCodeModeV1(_)
        | plakous_compose::CellDescriptor::CodexMinimalLocalCodeModeV1(_) => {
            return Err(invalid(
                "native Codex local composition forbids product-extension evidence",
            ));
        }
    };
    Ok(ExtensionFacts {
        target: profile
            .descriptor
            .outer_projection()
            .target()
            .integration_class(),
        catalog: profile.descriptor.nested_projection().digest(),
        presentation: profile.descriptor.outer_projection().digest(),
        limits,
    })
}

#[cfg(test)]
mod tests;
