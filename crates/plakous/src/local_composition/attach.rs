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
    pub(super) kind: LocalCompositionProfileKind,
    profile_identity: Digest,
    engine_contract_identity: Digest,
}

#[derive(Clone)]
pub(super) enum LocalCompositionProfileKind {
    CodexNativeLocalCodeModeV1(CodexNativeLocalCodeModeDescriptorV1),
    CodexMinimalLocalCodeModeV1(CodexMinimalLocalCodeModeDescriptorV1),
    ProductJavaScriptV1(ProductJavaScriptCompositionDescriptorV1),
    #[cfg(feature = "composition-python")]
    PythonPtcV1(PythonPtcDescriptorV1),
}

impl LocalCompositionProfileKind {
    /// Reports whether this profile is a product PTC lane that names a
    /// coding presentation kind.
    const fn requires_presentation_kind(&self) -> bool {
        match self {
            Self::CodexNativeLocalCodeModeV1(_) | Self::CodexMinimalLocalCodeModeV1(_) => false,
            Self::ProductJavaScriptV1(_) => true,
            #[cfg(feature = "composition-python")]
            Self::PythonPtcV1(_) => true,
        }
    }
}

impl LocalCompositionProfile {
    /// Codex `exec`/`wait` bound to a rquickjs sidecar host.
    ///
    /// # Errors
    ///
    /// Returns an identity error if the sidecar engine contract cannot be identified.
    pub fn codex_native_local_code_mode_host_v1(
        descriptor: &CodexNativeLocalCodeModeDescriptorV1,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            kind: LocalCompositionProfileKind::CodexNativeLocalCodeModeV1(descriptor.clone()),
            profile_identity: descriptor.identity(),
            engine_contract_identity: cell_engine_contract_identity(&plakous_cell::QUICKJS_WORKER)?,
        })
    }

    /// Code Mode `exec`/`wait` over the minimal nested coding catalog.
    ///
    /// # Errors
    ///
    /// Returns an identity error if the sidecar engine contract cannot be identified.
    pub fn codex_minimal_local_code_mode_host_v1(
        descriptor: &CodexMinimalLocalCodeModeDescriptorV1,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            kind: LocalCompositionProfileKind::CodexMinimalLocalCodeModeV1(descriptor.clone()),
            profile_identity: descriptor.identity(),
            engine_contract_identity: cell_engine_contract_identity(&plakous_cell::QUICKJS_WORKER)?,
        })
    }

    /// Selects one product-owned JavaScript profile over an exact Standard or Custom target.
    ///
    /// # Errors
    ///
    /// Returns an identity error if the pinned engine contract cannot be identified.
    pub fn product_javascript_v1(
        descriptor: &ProductJavaScriptCompositionDescriptorV1,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            kind: LocalCompositionProfileKind::ProductJavaScriptV1(descriptor.clone()),
            profile_identity: descriptor.identity(),
            engine_contract_identity: cell_engine_contract_identity(&plakous_cell::QUICKJS_WORKER)?,
        })
    }

    /// Selects the exact backend-independent local Python projection.
    ///
    /// # Errors
    ///
    /// Returns an identity error if the pinned Monty contract cannot be identified.
    #[cfg(feature = "composition-python")]
    pub fn python_ptc_v1(descriptor: &PythonPtcDescriptorV1) -> Result<Self, AgentError> {
        Ok(Self {
            kind: LocalCompositionProfileKind::PythonPtcV1(descriptor.clone()),
            profile_identity: descriptor.identity(),
            engine_contract_identity: cell_engine_contract_identity(&plakous_cell::MONTY_WORKER)?,
        })
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
        match self.kind {
            LocalCompositionProfileKind::CodexNativeLocalCodeModeV1(_)
            | LocalCompositionProfileKind::CodexMinimalLocalCodeModeV1(_)
            | LocalCompositionProfileKind::ProductJavaScriptV1(_) => {
                LocalCompositionLanguage::JavaScript
            }
            #[cfg(feature = "composition-python")]
            LocalCompositionProfileKind::PythonPtcV1(_) => LocalCompositionLanguage::Python,
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
    profile: LocalCompositionProfile,
    artifact: LocalCompositionEngineArtifact,
    binding_identity: Digest,
    evidence_receipt: Option<Digest>,
    evidence: Option<ProductExtensionEvidence>,
    presentation_kind: Option<plakous_tools::CodingPresentation>,
}

impl LocalCompositionAttachment {
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
        let evidence = match (&self.profile.kind, self.evidence) {
            (
                LocalCompositionProfileKind::CodexNativeLocalCodeModeV1(_)
                | LocalCompositionProfileKind::CodexMinimalLocalCodeModeV1(_),
                None,
            ) => None,
            (
                LocalCompositionProfileKind::CodexNativeLocalCodeModeV1(_)
                | LocalCompositionProfileKind::CodexMinimalLocalCodeModeV1(_),
                Some(_),
            ) => {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "native Codex local composition forbids product-extension evidence",
                ));
            }
            (LocalCompositionProfileKind::ProductJavaScriptV1(descriptor), Some(evidence)) => {
                validate_product_evidence(&self.profile, &artifact, descriptor, &evidence)?;
                Some(evidence)
            }
            (LocalCompositionProfileKind::ProductJavaScriptV1(_), None) => {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "product JavaScript local composition requires product-extension evidence",
                ));
            }
            #[cfg(feature = "composition-python")]
            (LocalCompositionProfileKind::PythonPtcV1(descriptor), Some(evidence)) => {
                validate_python_evidence(&self.profile, &artifact, descriptor, &evidence)?;
                Some(evidence)
            }
            #[cfg(feature = "composition-python")]
            (LocalCompositionProfileKind::PythonPtcV1(_), None) => {
                return Err(invalid(
                    "local Python composition requires product-extension evidence",
                ));
            }
        };
        if self.profile.kind.requires_presentation_kind() && self.presentation_kind.is_none() {
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
#[allow(clippy::too_many_lines, reason = "closed profile/artifact match table")]
pub fn attach_local_composition(
    builder: AgentBuilder,
    attachment: Option<LocalCompositionAttachment>,
) -> Result<AgentBuilder, AgentError> {
    let Some(attachment) = attachment else {
        return Ok(builder);
    };
    let LocalCompositionAttachment {
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
    match (profile.kind, artifact.kind, evidence) {
        (
            LocalCompositionProfileKind::CodexNativeLocalCodeModeV1(descriptor),
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                path,
                environment_identity,
            },
            None,
        ) => attach_codex_native_local_code_mode(
            builder,
            &descriptor,
            environment_identity,
            path,
            attachment_identity,
        ),
        (
            LocalCompositionProfileKind::CodexMinimalLocalCodeModeV1(descriptor),
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                path,
                environment_identity,
            },
            None,
        ) => attach_codex_minimal_local_code_mode(
            builder,
            &descriptor,
            environment_identity,
            path,
            attachment_identity,
        ),
        (
            LocalCompositionProfileKind::ProductJavaScriptV1(descriptor),
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                path,
                environment_identity,
            },
            Some(evidence),
        ) => {
            let presentation_kind = presentation_kind.ok_or_else(|| {
                invalid("product JavaScript attachment requires presentation kind")
            })?;
            attach_product_javascript(
                builder,
                &descriptor,
                path,
                environment_identity,
                attachment_identity,
                evidence,
                presentation_kind,
            )
        }
        #[cfg(feature = "composition-python")]
        (
            LocalCompositionProfileKind::PythonPtcV1(descriptor),
            LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
                path,
                environment_identity,
            },
            Some(evidence),
        ) => {
            let presentation_kind = presentation_kind
                .ok_or_else(|| invalid("product Python attachment requires presentation kind"))?;
            attach_python(
                builder,
                &descriptor,
                path,
                environment_identity,
                attachment_identity,
                evidence,
                presentation_kind,
            )
        }
        _ => Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "local composition profile and product evidence lane disagree",
        )),
    }
}

/// Attaches `exec`/`wait` Code Mode on a digest-pinned `plakous-js-host` sidecar.
///
/// The worker path must be absolute and already canonical, without symlinks.
/// Attachment records its SHA-256; it does not certify worker provenance or
/// imply that the re-check before spawning is an atomic execution guarantee.
///
/// # Errors
///
/// Fails when the sidecar path or exact descriptor fails closed admission.
pub fn codex_minimal_local_code_mode_sidecar(
    builder: AgentBuilder,
    descriptor: &CodexMinimalLocalCodeModeDescriptorV1,
    host_path: impl Into<PathBuf>,
) -> Result<AgentBuilder, AgentError> {
    let profile = LocalCompositionProfile::codex_minimal_local_code_mode_host_v1(descriptor)?;
    let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(host_path)?;
    let attachment = LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .build()?;
    attach_local_composition(builder, Some(attachment))
}

fn attach_codex_native_local_code_mode(
    builder: AgentBuilder,
    descriptor: &CodexNativeLocalCodeModeDescriptorV1,
    identity: Vec<u8>,
    sidecar: PathBuf,
    attachment: LocalCompositionAttachmentIdentity,
) -> Result<AgentBuilder, AgentError> {
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(CellIntegration {
        worker: sidecar,
        identity,
        presentation: Box::new(JavaScriptPresentation::CodexExact),
    });
    let descriptor = neutral_descriptor(descriptor)
        .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor,
        integration,
        attachment,
    })
}

fn attach_codex_minimal_local_code_mode(
    builder: AgentBuilder,
    descriptor: &CodexMinimalLocalCodeModeDescriptorV1,
    identity: Vec<u8>,
    sidecar: PathBuf,
    attachment: LocalCompositionAttachmentIdentity,
) -> Result<AgentBuilder, AgentError> {
    let nested_catalog = descriptor
        .nested_tools()
        .iter()
        .map(|tool| tool.exposed_name().clone())
        .collect();
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(CellIntegration {
        worker: sidecar,
        identity,
        presentation: Box::new(JavaScriptPresentation::CodexMinimal { nested_catalog }),
    });
    let descriptor = neutral_minimal_descriptor(descriptor)
        .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor,
        integration,
        attachment,
    })
}
