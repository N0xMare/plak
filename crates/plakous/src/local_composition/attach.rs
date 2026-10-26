//! Composition profiles, attachments and the attach entry points.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

/// Immutable, exact presentation data selected for one agent.
///
/// Construction is intentionally limited to profiles defined and qualified by
/// Plakous. It contains no evaluator, dispatcher, registry, or operation authority.
#[derive(Clone)]
pub struct LocalCompositionProfile {
    pub(super) descriptor: plakous_compose::CellDescriptor,
    profile_identity: Digest,
    engine_contract_identity: Digest,
}

impl LocalCompositionProfile {
    /// Selects one exact compiled cell presentation and its required worker contract.
    ///
    /// Retired per-engine constructors are not aliases for this entry point:
    ///
    /// ```compile_fail,E0599
    /// use plakous::agent::LocalCompositionProfile;
    /// use plakous::tools::PythonPtcDescriptorV1;
    /// fn obsolete(descriptor: &PythonPtcDescriptorV1) {
    ///     let _ = LocalCompositionProfile::python_ptc_v1(descriptor);
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns an identity error if the required worker contract cannot be identified.
    pub fn cell(descriptor: &plakous_compose::CellDescriptor) -> Result<Self, AgentError> {
        let worker = match descriptor {
            plakous_compose::CellDescriptor::PythonPtcV1(_)
            | plakous_compose::CellDescriptor::PythonHarnessSdkV1(_) => &plakous_cell::MONTY_WORKER,
            plakous_compose::CellDescriptor::CodexNativeLocalCodeModeV1(_)
            | plakous_compose::CellDescriptor::CodexMinimalLocalCodeModeV1(_)
            | plakous_compose::CellDescriptor::ProductJavaScriptV1(_) => {
                &plakous_cell::QUICKJS_WORKER
            }
        };
        Ok(Self {
            descriptor: descriptor.clone(),
            profile_identity: descriptor.identity(),
            engine_contract_identity: cell_engine_contract_identity(worker)?,
        })
    }

    const fn requires_presentation_kind(&self) -> bool {
        matches!(
            self.descriptor,
            plakous_compose::CellDescriptor::ProductJavaScriptV1(_)
                | plakous_compose::CellDescriptor::PythonPtcV1(_)
        )
    }

    /// Returns this profile's exact semantic identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.profile_identity
    }

    /// Returns the exact evaluator contract required by this profile.
    #[must_use]
    pub const fn engine_contract_identity(&self) -> Digest {
        self.engine_contract_identity
    }

    /// Returns the exact source language, without implying cross-language compatibility.
    #[must_use]
    pub const fn language(&self) -> LocalCompositionLanguage {
        match self.descriptor {
            plakous_compose::CellDescriptor::CodexNativeLocalCodeModeV1(_)
            | plakous_compose::CellDescriptor::CodexMinimalLocalCodeModeV1(_)
            | plakous_compose::CellDescriptor::ProductJavaScriptV1(_) => {
                LocalCompositionLanguage::JavaScript
            }
            plakous_compose::CellDescriptor::PythonPtcV1(_)
            | plakous_compose::CellDescriptor::PythonHarnessSdkV1(_) => {
                LocalCompositionLanguage::Python
            }
        }
    }
}

impl fmt::Debug for LocalCompositionProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalCompositionProfile")
            .field("identity", &self.profile_identity)
            .field("engine_contract_identity", &self.engine_contract_identity)
            .field("language", &self.language())
            .finish_non_exhaustive()
    }
}

/// One exact profile-to-engine binding for an agent.
pub struct LocalCompositionAttachment {
    #[cfg(test)]
    sdk_foreground_budget: Option<std::time::Duration>,
    profile: LocalCompositionProfile,
    artifact: LocalCompositionEngineArtifact,
    binding_identity: Digest,
    evidence_receipt: Option<Digest>,
    evidence: Option<ProductExtensionEvidence>,
    presentation_kind: Option<plakous_tools::CodingPresentation>,
}

impl LocalCompositionAttachment {
    #[cfg(test)]
    pub(crate) fn with_sdk_foreground_budget(mut self, budget: std::time::Duration) -> Self {
        self.sdk_foreground_budget = Some(budget);
        self
    }
    /// Starts a builder for one exact profile binding.
    #[must_use]
    pub fn builder(profile: LocalCompositionProfile) -> LocalCompositionAttachmentBuilder {
        LocalCompositionAttachmentBuilder {
            profile,
            artifact: None,
            evidence: None,
            presentation_kind: None,
            duplicate_artifact: false,
            duplicate_evidence: false,
        }
    }

    /// Returns the exact profile-to-artifact binding identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.binding_identity
    }

    /// Returns the selected immutable profile.
    #[must_use]
    pub const fn profile(&self) -> &LocalCompositionProfile {
        &self.profile
    }

    /// Returns the selected verified engine artifact.
    #[must_use]
    pub const fn artifact(&self) -> &LocalCompositionEngineArtifact {
        &self.artifact
    }
}

impl fmt::Debug for LocalCompositionAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalCompositionAttachment")
            .field("identity", &self.binding_identity)
            .field("profile", &self.profile)
            .field("artifact", &self.artifact)
            .finish_non_exhaustive()
    }
}

/// Builder that requires exactly one verified artifact for one exact profile.
pub struct LocalCompositionAttachmentBuilder {
    profile: LocalCompositionProfile,
    artifact: Option<LocalCompositionEngineArtifact>,
    evidence: Option<ProductExtensionEvidence>,
    presentation_kind: Option<plakous_tools::CodingPresentation>,
    duplicate_artifact: bool,
    duplicate_evidence: bool,
}

impl LocalCompositionAttachmentBuilder {
    /// Supplies the sole evaluator artifact for this binding.
    #[must_use]
    pub fn engine_artifact(mut self, artifact: LocalCompositionEngineArtifact) -> Self {
        if self.artifact.is_some() {
            self.duplicate_artifact = true;
        } else {
            self.artifact = Some(artifact);
        }
        self
    }

    /// Supplies the vertical Direct pack used to compile nested catalogs.
    #[must_use]
    pub fn coding_presentation_kind(mut self, kind: plakous_tools::CodingPresentation) -> Self {
        self.presentation_kind = Some(kind);
        self
    }

    /// Supplies the independent receipt required by a product-owned profile.
    #[must_use]
    pub fn product_extension_evidence(mut self, evidence: ProductExtensionEvidence) -> Self {
        if self.evidence.is_some() {
            self.duplicate_evidence = true;
        } else {
            self.evidence = Some(evidence);
        }
        self
    }

    /// Closes the exact profile-to-artifact binding.
    ///
    /// # Errors
    ///
    /// Rejects a missing artifact or an artifact implementing a different exact
    /// engine contract. No language or backend fallback is attempted.
    pub fn build(self) -> Result<LocalCompositionAttachment, AgentError> {
        if self.duplicate_artifact || self.duplicate_evidence {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "local composition attachment received more than one engine artifact",
            ));
        }
        let artifact = self.artifact.ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "local composition attachment requires exactly one engine artifact",
            )
        })?;
        validate_engine_contract_binding(
            self.profile.engine_contract_identity,
            artifact.engine_contract_identity,
        )?;
        let evidence = match (&self.profile.descriptor, self.evidence) {
            (
                plakous_compose::CellDescriptor::CodexNativeLocalCodeModeV1(_)
                | plakous_compose::CellDescriptor::CodexMinimalLocalCodeModeV1(_),
                None,
            ) => None,
            (
                plakous_compose::CellDescriptor::CodexNativeLocalCodeModeV1(_)
                | plakous_compose::CellDescriptor::CodexMinimalLocalCodeModeV1(_),
                Some(_),
            ) => {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "native Codex local composition forbids product-extension evidence",
                ));
            }
            (
                plakous_compose::CellDescriptor::ProductJavaScriptV1(_)
                | plakous_compose::CellDescriptor::PythonPtcV1(_)
                | plakous_compose::CellDescriptor::PythonHarnessSdkV1(_),
                Some(evidence),
            ) => {
                validate_product_evidence(&self.profile, &artifact, &evidence)?;
                Some(evidence)
            }
            (
                plakous_compose::CellDescriptor::ProductJavaScriptV1(_)
                | plakous_compose::CellDescriptor::PythonPtcV1(_)
                | plakous_compose::CellDescriptor::PythonHarnessSdkV1(_),
                None,
            ) => {
                return Err(invalid(
                    "product local composition requires product-extension evidence",
                ));
            }
        };
        if self.profile.requires_presentation_kind() && self.presentation_kind.is_none() {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "product PTC local composition requires a coding presentation kind",
            ));
        }
        let evidence_receipt = evidence.as_ref().map(ProductExtensionEvidence::identity);
        let binding_identity = attachment_binding_identity(
            self.profile.profile_identity,
            artifact.artifact_identity,
            self.profile.engine_contract_identity,
            evidence_receipt,
        )?;
        Ok(LocalCompositionAttachment {
            #[cfg(test)]
            sdk_foreground_budget: None,
            profile: self.profile,
            artifact,
            binding_identity,
            evidence_receipt,
            evidence,
            presentation_kind: self.presentation_kind,
        })
    }
}

pub(super) fn attachment_binding_identity(
    profile: Digest,
    artifact: Digest,
    engine_contract: Digest,
    evidence: Option<Digest>,
) -> Result<Digest, AgentError> {
    let profile_bytes = profile.identity_bytes();
    let artifact_bytes = artifact.identity_bytes();
    let engine_contract_bytes = engine_contract.identity_bytes();
    let evidence_bytes = evidence.map(|value| value.identity_bytes());
    let mut parts = vec![
        b"plakous.local-composition.attachment.v1".as_slice(),
        profile_bytes.as_slice(),
        artifact_bytes.as_slice(),
        engine_contract_bytes.as_slice(),
    ];
    if let Some(value) = evidence_bytes.as_ref() {
        parts.push(value.as_slice());
    }
    Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &parts)
        .map_err(|error| AgentError::new(AgentErrorKind::Identity, error.to_string()))
}

/// Attaches zero or one exact local-composition binding to an agent builder.
///
/// `None` is dormant and semantically absent: it does not change tool exposure,
/// engine state, environment identity, or durable obligations.
///
/// # Errors
///
/// Fails closed if the binding is not exact for the builder or one was already attached.
pub fn attach_local_composition(
    builder: AgentBuilder,
    attachment: Option<LocalCompositionAttachment>,
) -> Result<AgentBuilder, AgentError> {
    let Some(attachment) = attachment else {
        return Ok(builder);
    };
    let LocalCompositionAttachment {
        #[cfg(test)]
        sdk_foreground_budget,
        profile,
        artifact,
        binding_identity,
        evidence_receipt,
        evidence,
        presentation_kind,
    } = attachment;
    let attachment_identity = LocalCompositionAttachmentIdentity {
        profile: profile.profile_identity,
        artifact: artifact.artifact_identity,
        binding: binding_identity,
        engine_contract: profile.engine_contract_identity,
        evidence_receipt,
    };
    let (descriptor, presentation): (_, Box<dyn integration::Presentation>) =
        match (profile.descriptor, evidence) {
            (plakous_compose::CellDescriptor::CodexNativeLocalCodeModeV1(descriptor), None) => (
                neutral_descriptor(&descriptor).map_err(invalid)?,
                Box::new(JavaScriptPresentation::CodexExact),
            ),
            (plakous_compose::CellDescriptor::CodexMinimalLocalCodeModeV1(descriptor), None) => {
                let nested_catalog = descriptor
                    .nested_tools()
                    .iter()
                    .map(|tool| tool.exposed_name().clone())
                    .collect();
                (
                    neutral_minimal_descriptor(&descriptor).map_err(invalid)?,
                    Box::new(JavaScriptPresentation::CodexMinimal { nested_catalog }),
                )
            }
            (plakous_compose::CellDescriptor::ProductJavaScriptV1(descriptor), Some(evidence)) => {
                let presentation_kind = presentation_kind.ok_or_else(|| {
                    invalid("product JavaScript attachment requires presentation kind")
                })?;
                (
                    neutral_product_descriptor(&descriptor).map_err(invalid)?,
                    Box::new(product_javascript_presentation(
                        &descriptor,
                        evidence,
                        presentation_kind,
                    )?),
                )
            }
            (plakous_compose::CellDescriptor::PythonHarnessSdkV1(descriptor), Some(evidence)) => (
                sdk::descriptor(&descriptor).map_err(invalid)?,
                Box::new(sdk::SdkPresentation {
                    budget: {
                        let budget = std::time::Duration::from_millis(
                            plakous_compose::PythonHarnessSdkDescriptorV1::FOREGROUND_MS,
                        );
                        #[cfg(test)]
                        let budget = sdk_foreground_budget.unwrap_or(budget);
                        budget
                    },
                    evidence,
                    api: descriptor.host_api().clone(),
                }),
            ),
            (plakous_compose::CellDescriptor::PythonPtcV1(descriptor), Some(evidence)) => {
                let presentation_kind = presentation_kind.ok_or_else(|| {
                    invalid("product Python attachment requires presentation kind")
                })?;
                (
                    neutral_python_descriptor(&descriptor).map_err(invalid)?,
                    Box::new(python::python_presentation(
                        &descriptor,
                        evidence,
                        presentation_kind,
                    )?),
                )
            }
            _ => {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "local composition profile and product evidence lane disagree",
                ));
            }
        };
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(CellIntegration {
        identity: artifact.worker.environment_identity(),
        worker: artifact.worker.path,
        presentation,
    });
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor,
        integration,
        attachment: attachment_identity,
    })
}
