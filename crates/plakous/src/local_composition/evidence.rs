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
mod tests {
    use super::*;
    use plakous_compose::{CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn profiles() -> TestResult<Vec<LocalCompositionProfile>> {
        let mut registry = plakous_tools::ToolRegistryBuilder::default();
        plakous_tools::register_packs(
            &plakous_compose::standard_coding_packs_in_memory()?,
            &mut registry,
        )?;
        CodexNativeLocalCodeModeV1::register_markers_into(&mut registry)?;
        ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
        PythonPtcV1::register_markers_into(&mut registry)?;
        let registry = registry.freeze();
        let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"cell-attachment-matrix"],
        )?)?)?;
        let target = ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "openai.codex.responses",
            "openai.codex_subscription",
            "openai.codex.compat.0.153.3",
            "responses-family",
            "gpt-codex-code-mode-test",
            "gpt-codex-code-mode-test",
            configuration,
            PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        let job = plakous_tools::JobConfig::default();
        Ok(vec![
            LocalCompositionProfile::cell(
                &(&CodexNativeLocalCodeModeV1::compile(job, registry.clone(), target.clone())?)
                    .into(),
            )?,
            LocalCompositionProfile::cell(
                &(&CodexMinimalLocalCodeModeV1::compile(job, registry.clone(), target.clone())?)
                    .into(),
            )?,
            LocalCompositionProfile::cell(
                &(&ProductJavaScriptCompositionV1::compile_for_kind(
                    job,
                    registry.clone(),
                    target.clone(),
                    plakous_tools::CodingPresentation::Codex,
                    [ToolName::new("read_file")?],
                )?)
                    .into(),
            )?,
            LocalCompositionProfile::cell(
                &(&PythonPtcV1::compile_for_kind(
                    job,
                    registry,
                    target,
                    plakous_compose::PythonPtcPresentationV1::StructuredFunctions,
                    plakous_tools::CodingPresentation::Codex,
                    [ToolName::new("read_file")?],
                )?)
                    .into(),
            )?,
        ])
    }

    fn evidence(
        profile: &LocalCompositionProfile,
        artifact: &LocalCompositionEngineArtifact,
    ) -> TestResult<ProductExtensionEvidence> {
        let facts = extension_facts(profile)?;
        let identity = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"receipt"])?;
        Ok(ProductExtensionEvidence {
            target: facts.target,
            profile: profile.identity(),
            artifact: artifact.identity(),
            catalog: facts.catalog,
            presentation: facts.presentation,
            limits: facts.limits,
            host: artifact.host_identity(),
            engine_contract: artifact.engine_contract_identity(),
            containment: artifact.containment().identity(),
            product_issuer: identity,
            qualification_receipt: identity,
            receipt: identity,
            selected_configuration: serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"matrix"],
            )?)?)?,
            effective_configuration: serde_json::from_value(serde_json::to_value(
                Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"matrix"],
                )?,
            )?)?,
        })
    }

    #[test]
    fn cell_profiles_preserve_the_complete_descriptor_and_projection_evidence() -> TestResult {
        for (index, profile) in profiles()?.into_iter().enumerate() {
            assert_eq!(profile.identity(), profile.descriptor.identity());
            assert_eq!(profile.identity().version(), 3);
            assert_eq!(
                profile.language(),
                if index == 3 {
                    LocalCompositionLanguage::Python
                } else {
                    LocalCompositionLanguage::JavaScript
                }
            );
            if index >= 2 {
                let facts = extension_facts(&profile)?;
                assert_eq!(
                    facts.catalog,
                    profile.descriptor.nested_projection().digest()
                );
                assert_eq!(
                    facts.presentation,
                    profile.descriptor.outer_projection().digest()
                );
            } else {
                assert!(extension_facts(&profile).is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn attachment_accepts_only_the_exact_profile_engine_and_evidence_lane() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("worker");
        std::fs::write(&path, b"offline matrix worker")?;
        let path = path.canonicalize()?;
        let profiles = profiles()?;
        for (index, profile) in profiles.iter().enumerate() {
            for python in [false, true] {
                for with_receipt in [false, true] {
                    let artifact = if python {
                        LocalCompositionEngineArtifact::monty_worker_v1(&path)?
                    } else {
                        LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?
                    };
                    let receipt = if with_receipt {
                        Some(evidence(
                            if index >= 2 { profile } else { &profiles[2] },
                            &artifact,
                        )?)
                    } else {
                        None
                    };
                    let mut builder = LocalCompositionAttachment::builder(profile.clone())
                        .engine_artifact(artifact)
                        .coding_presentation_kind(plakous_tools::CodingPresentation::Codex);
                    if let Some(receipt) = receipt {
                        builder = builder.product_extension_evidence(receipt);
                    }
                    assert_eq!(
                        builder.build().is_ok(),
                        python == (index == 3) && with_receipt == (index >= 2),
                        "profile {index}, python {python}, receipt {with_receipt}"
                    );
                }
            }
            assert!(
                LocalCompositionAttachment::builder(profile.clone())
                    .build()
                    .is_err()
            );
            if index >= 2 {
                let artifact = if index == 3 {
                    LocalCompositionEngineArtifact::monty_worker_v1(&path)?
                } else {
                    LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?
                };
                let receipt = evidence(profile, &artifact)?;
                assert!(
                    LocalCompositionAttachment::builder(profile.clone())
                        .engine_artifact(artifact)
                        .product_extension_evidence(receipt)
                        .build()
                        .is_err(),
                    "missing presentation kind"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn every_attachment_evidence_fact_is_checked_for_both_languages() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("worker");
        std::fs::write(&path, b"offline evidence worker")?;
        let path = path.canonicalize()?;
        for (index, profile) in profiles()?.into_iter().enumerate().skip(2) {
            let artifact = if index == 3 {
                LocalCompositionEngineArtifact::monty_worker_v1(&path)?
            } else {
                LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?
            };
            let receipt = evidence(&profile, &artifact)?;
            assert!(validate_product_evidence(&profile, &artifact, &receipt).is_ok());
            let neighbor = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"neighbor"])?;
            for axis in 0..11 {
                let mut drifted = receipt.clone();
                match axis {
                    0 => drifted.target = IntegrationClass::Custom,
                    1 => drifted.profile = neighbor,
                    2 => drifted.artifact = neighbor,
                    3 => drifted.catalog = neighbor,
                    4 => drifted.presentation = neighbor,
                    5 => drifted.host = neighbor,
                    6 => drifted.engine_contract = neighbor,
                    7 => drifted.containment = neighbor,
                    8 => drifted.limits.max_source_bytes += 1,
                    9 => drifted.limits.max_yield_time_ms += 1,
                    _ => drifted.limits.max_output_tokens += 1,
                }
                assert!(
                    validate_product_evidence(&profile, &artifact, &drifted).is_err(),
                    "profile {index}, axis {axis}"
                );
            }
        }
        Ok(())
    }
}
