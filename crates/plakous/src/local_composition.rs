//! Capability-poor attachment for driver-owned local composition.
//!
//! The facade deliberately exposes no session, bridge, responder, executor, or
//! operation trait. Those remain private driver authority:
//!
//! ```compile_fail
//! use plakous::agent::LocalCompositionSession;
//! ```
//!
//! Exact DTO fields cannot be forged by a consumer:
//!
//! ```compile_fail
//! use plakous::agent::LocalCompositionProfile;
//! fn forge() -> LocalCompositionProfile {
//!     LocalCompositionProfile { kind: todo!(), profile_identity: todo!(), engine_contract_identity: todo!() }
//! }
//! ```

#[cfg(feature = "composition-python")]
use std::collections::BTreeMap;
use std::{
    fmt,
    fs::File,
    future::Future,
    io::Read,
    os::unix::ffi::OsStringExt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use plakous_agent::internal::{
    AgentBuilder, AgentError, AgentErrorKind, LocalCompositionAttachmentIdentity,
    LocalCompositionConstruction, LocalCompositionContentItem, LocalCompositionDescriptor,
    LocalCompositionExecuteInput, LocalCompositionFuture, LocalCompositionImageDetail,
    LocalCompositionIntegration, LocalCompositionLifecycle, LocalCompositionResultProjection,
    LocalCompositionRuntimeContract, LocalCompositionSessionBundle, LocalCompositionShutdownResult,
    LocalCompositionToolDefinition, LocalCompositionWaitInput,
};
use plakous_code_mode as native;
use plakous_compose::{
    CodexMinimalLocalCodeModeDescriptorV1, CodexNativeLocalCodeModeDescriptorV1,
    ProductJavaScriptCompositionDescriptorV1, ProductJavaScriptCompositionV1,
};
#[cfg(feature = "composition-python")]
use plakous_compose::{PythonPtcDescriptorV1, PythonPtcLimitsV1, PythonPtcV1};
use plakous_model::{
    CompiledProviderConfiguration, Digest, DigestDomain, EffectiveProviderConfigurationId,
    IntegrationClass, SelectedProviderConfigurationId, ToolExecutionDisposition, ToolName,
};
use plakous_tools::{
    FrozenToolRegistry, PROCESS_TOOL_PROJECTION_REVISION,
    PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1, PRODUCT_JAVASCRIPT_RUN_NAME_V1,
    PRODUCT_JAVASCRIPT_WAIT_NAME_V1, ProviderModelProjectionIdentityV2, ToolExecutorId,
    ToolProjectionMap,
};
#[cfg(feature = "composition-python")]
use plakous_tools::{
    PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1, PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_WAIT_NAME_V1,
};

const PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS: u64 = 10_000;
const PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS: u64 = 10_000;
#[cfg(feature = "composition-python")]
const PYTHON_DEFAULT_YIELD_TIME_MS: u64 = 10_000;
/// Maximum UTF-8 source bytes in one cell.
#[cfg(feature = "composition-python")]
const PYTHON_SOURCE_MAX_BYTES: usize = 256 * 1024;

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

/// Immutable, exact presentation data selected for one agent.
///
/// Construction is intentionally limited to profiles defined and qualified by
/// Plakous. It contains no evaluator, dispatcher, registry, or operation authority.
#[derive(Clone)]
pub struct LocalCompositionProfile {
    kind: LocalCompositionProfileKind,
    profile_identity: Digest,
    engine_contract_identity: Digest,
}

#[derive(Clone)]
enum LocalCompositionProfileKind {
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
            engine_contract_identity: rquickjs_sidecar_engine_contract_identity()?,
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
            engine_contract_identity: rquickjs_sidecar_engine_contract_identity()?,
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
            engine_contract_identity: rquickjs_sidecar_engine_contract_identity()?,
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
            engine_contract_identity: monty_engine_contract_identity()?,
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

/// One verified out-of-process evaluator artifact.
///
/// The value carries only exact artifact facts and a held verified executable.
/// Process launch, bridge supervision, nested dispatch, and cleanup remain in Plakous.
/// Truthful containment facts for one verified evaluator artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalCompositionContainment {
    identity: Digest,
}

impl LocalCompositionContainment {
    /// Returns the semantic identity of these exact containment claims.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }

    /// The evaluator is isolated in its own joined process.
    #[must_use]
    pub const fn is_process_isolated(self) -> bool {
        true
    }

    /// The exact host makes no operating-system sandbox claim.
    #[must_use]
    pub const fn claims_os_sandbox(self) -> bool {
        false
    }
}

/// One verified out-of-process evaluator artifact.
///
/// It carries no provider credential, executor, store, or dispatch authority.
pub struct LocalCompositionEngineArtifact {
    kind: LocalCompositionEngineArtifactKind,
    artifact_identity: Digest,
    engine_contract_identity: Digest,
    host_identity: Digest,
    containment: LocalCompositionContainment,
}

enum LocalCompositionEngineArtifactKind {
    RquickjsSidecarV1 {
        path: PathBuf,
        environment_identity: Vec<u8>,
    },
    #[cfg(feature = "composition-python")]
    MontyPythonPtcV1 {
        path: PathBuf,
        environment_identity: Vec<u8>,
    },
}

impl LocalCompositionEngineArtifact {
    /// Digest-pinned `monty` worker for product py-ptc.
    ///
    /// # Errors
    ///
    /// Fails when the path is not a readable canonical regular file.
    #[cfg(feature = "composition-python")]
    pub fn monty_pool_v1(worker_path: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let (canonical, _dev, _ino, sha) = file_identity(&worker_path.into())?;
        let environment_identity = [
            b"monty-pool".as_slice(),
            canonical.as_os_str().as_encoded_bytes(),
            &sha,
        ]
        .concat();
        let artifact_identity =
            digest_environment(b"plakous.monty-engine-artifact.v1", &environment_identity)?;
        let host_identity =
            digest_environment(b"plakous.monty-verified-host.v1", &environment_identity)?;
        let containment = LocalCompositionContainment {
            identity: Digest::sha256(
                DigestDomain::ExecutionEnvironment,
                1,
                &[
                    b"plakous.monty-containment.v1",
                    b"sidecar-monty-pool",
                    b"no-os-sandbox-claim",
                    &host_identity.identity_bytes(),
                ],
            )
            .map_err(identity_error)?,
        };
        Ok(Self {
            kind: LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
                path: canonical,
                environment_identity,
            },
            artifact_identity,
            engine_contract_identity: monty_engine_contract_identity()?,
            host_identity,
            containment,
        })
    }

    /// Records a caller-supplied JavaScript sidecar canonical path and SHA-256.
    ///
    /// The absolute path must already be canonical, with no symlink components.
    /// This records the regular file at attachment, not proof of worker provenance.
    /// The path and hash are re-checked immediately before each evaluator spawn.
    ///
    /// # Errors
    ///
    /// Fails when the path is not canonical or is not a readable regular file.
    pub fn rquickjs_sidecar_v1(host_path: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let (canonical, _dev, _ino, sha) = file_identity(&host_path.into())?;
        let environment_identity = [
            b"rquickjs-host".as_slice(),
            canonical.as_os_str().as_encoded_bytes(),
            &sha,
        ]
        .concat();
        let artifact_identity = digest_environment(
            b"plakous.local-composition.engine-artifact.v1",
            &environment_identity,
        )?;
        let host_identity = digest_environment(
            b"plakous.local-composition.verified-host.v1",
            &environment_identity,
        )?;
        let containment = LocalCompositionContainment {
            identity: Digest::sha256(
                DigestDomain::ExecutionEnvironment,
                1,
                &[
                    b"plakous.local-composition.containment.v1",
                    b"sidecar-rquickjs",
                    b"no-os-sandbox-claim",
                    &host_identity.identity_bytes(),
                ],
            )
            .map_err(identity_error)?,
        };
        Ok(Self {
            kind: LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                path: canonical,
                environment_identity,
            },
            artifact_identity,
            engine_contract_identity: rquickjs_sidecar_engine_contract_identity()?,
            host_identity,
            containment,
        })
    }

    /// Returns the exact verified artifact identity, including the executable path and hash.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.artifact_identity
    }

    /// Returns the exact evaluator contract implemented by this artifact.
    #[must_use]
    pub const fn engine_contract_identity(&self) -> Digest {
        self.engine_contract_identity
    }

    /// Returns the verified executable host identity bound separately by extension evidence.
    #[must_use]
    pub const fn host_identity(&self) -> Digest {
        self.host_identity
    }

    /// Returns truthful process-isolation facts without claiming an OS sandbox.
    #[must_use]
    pub const fn containment(&self) -> LocalCompositionContainment {
        self.containment
    }

    /// Returns the verified canonical executable path.
    #[must_use]
    pub fn path(&self) -> &Path {
        match &self.kind {
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 { path, .. } => path,
            #[cfg(feature = "composition-python")]
            LocalCompositionEngineArtifactKind::MontyPythonPtcV1 { path, .. } => path,
        }
    }
}

impl fmt::Debug for LocalCompositionEngineArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalCompositionEngineArtifact")
            .field("identity", &self.artifact_identity)
            .field("engine_contract_identity", &self.engine_contract_identity)
            .field("path", &self.path())
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

fn attachment_binding_identity(
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

fn validate_product_evidence(
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

fn product_javascript_limits_are_exact(limits: ProductExtensionLimits) -> bool {
    u64::try_from(native::MAX_SOURCE_BYTES).is_ok_and(|max_source_bytes| {
        limits.max_source_bytes == max_source_bytes
            && limits.max_yield_time_ms == PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS
            && limits.max_output_tokens == PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS
    })
}

fn validate_engine_contract_binding(profile: Digest, artifact: Digest) -> Result<(), AgentError> {
    if profile != artifact {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "local composition profile and engine artifact identities do not match",
        ));
    }
    Ok(())
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

fn rquickjs_sidecar_engine_contract_identity() -> Result<Digest, AgentError> {
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[
            b"plakous.local-composition.product-javascript.v1",
            b"rquickjs-host",
            plakous_js_eval::BOOTSTRAP_REVISION.as_bytes(),
        ],
    )
    .map_err(|error| AgentError::new(AgentErrorKind::Identity, error.to_string()))
}

pub(crate) fn restore_stored_artifact(
    stored: &plakous_agent::internal::facade_support::StoredCompositionArtifact,
) -> Result<LocalCompositionEngineArtifact, AgentError> {
    let refused = || {
        AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "recorded evaluator path, bytes or contract is unavailable or inconsistent; no inference was dispatched",
        )
    };
    let (prefix, label) = if stored.host.starts_with(b"rquickjs-host") {
        (
            b"rquickjs-host".as_slice(),
            b"plakous.local-composition.engine-artifact.v1".as_slice(),
        )
    } else if stored.host.starts_with(b"monty-pool") {
        (
            b"monty-pool".as_slice(),
            b"plakous.monty-engine-artifact.v1".as_slice(),
        )
    } else {
        return Err(refused());
    };
    // Product PTC appends one evidence digest; Code Mode does not. Select the
    // framing by the recorded artifact digest, never by probing candidate files.
    let evidence_len = rquickjs_sidecar_engine_contract_identity()?
        .identity_bytes()
        .len();
    let host = [
        Some(stored.host.as_slice()),
        stored
            .host
            .len()
            .checked_sub(evidence_len)
            .map(|len| &stored.host[..len]),
    ]
    .into_iter()
    .flatten()
    .find(|host| {
        digest_environment(label, host)
            .is_ok_and(|digest| digest.identity_bytes() == stored.artifact_identity)
    })
    .ok_or_else(refused)?;
    let mut binding_parts = vec![
        b"plakous.local-composition.attachment.v1".as_slice(),
        stored.profile_identity.as_slice(),
        stored.artifact_identity.as_slice(),
        stored.engine_contract_identity.as_slice(),
    ];
    if host.len() != stored.host.len() {
        let receipt = &stored.host[host.len()..];
        let digest = Digest::from_sha256_bytes(
            DigestDomain::ExecutionEnvironment,
            1,
            receipt[receipt.len() - 32..]
                .try_into()
                .map_err(|_| refused())?,
        )
        .map_err(|_| refused())?;
        if digest.identity_bytes() != receipt {
            return Err(refused());
        }
        binding_parts.push(receipt);
    }
    if Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &binding_parts)
        .map_err(|_| refused())?
        .identity_bytes()
        != stored.binding_identity
    {
        return Err(refused());
    }
    let path_end = host
        .len()
        .checked_sub(32)
        .filter(|end| *end > prefix.len())
        .ok_or_else(refused)?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(
        host[prefix.len()..path_end].to_vec(),
    ));
    if !path.is_absolute()
        || std::fs::canonicalize(&path)
            .map_err(|_| refused())?
            .as_os_str()
            != path.as_os_str()
    {
        return Err(refused());
    }
    let artifact = if prefix == b"rquickjs-host" {
        LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&path).map_err(|_| refused())?
    } else {
        #[cfg(feature = "composition-python")]
        {
            LocalCompositionEngineArtifact::monty_pool_v1(&path).map_err(|_| refused())?
        }
        #[cfg(not(feature = "composition-python"))]
        {
            return Err(refused());
        }
    };
    if artifact.identity().identity_bytes() != stored.artifact_identity
        || artifact.engine_contract_identity().identity_bytes() != stored.engine_contract_identity
    {
        return Err(refused());
    }
    Ok(artifact)
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::new(AgentErrorKind::InvalidInput, message.into())
}

fn identity_error(error: plakous_model::DigestError) -> AgentError {
    AgentError::new(AgentErrorKind::Identity, error.to_string())
}

fn digest_environment(label: &[u8], facts: &[u8]) -> Result<Digest, AgentError> {
    Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[label, facts]).map_err(identity_error)
}

#[cfg(feature = "composition-python")]
fn monty_engine_contract_identity() -> Result<Digest, AgentError> {
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[
            b"plakous.monty-python-engine-contract.v2",
            b"monty-pool",
            b"1.2.0",
            crate::monty_python::ADAPTER_REVISION.as_bytes(),
        ],
    )
    .map_err(identity_error)
}

pub(crate) fn verify_sidecar_before_spawn(path: &Path, expected: &[u8]) -> Result<(), String> {
    let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(path)
        .map_err(|error| error.to_string())?;
    match artifact.kind {
        LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
            environment_identity,
            ..
        } if environment_identity == expected => Ok(()),
        _ => Err("JavaScript sidecar identity changed after attachment".to_owned()),
    }
}

fn file_identity(path: &Path) -> Result<(PathBuf, u64, u64, [u8; 32]), AgentError> {
    use sha2::Digest as _;
    let canonical = path
        .canonicalize()
        .map_err(|error| invalid(error.to_string()))?;
    if canonical != path {
        return Err(invalid(
            "composition executable path must already be canonical",
        ));
    }
    let mut file = File::open(&canonical).map_err(|error| invalid(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| invalid(error.to_string()))?;
    if !metadata.is_file() {
        return Err(invalid("composition executable must be a regular file"));
    }
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| invalid(error.to_string()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok((
        canonical,
        metadata.dev(),
        metadata.ino(),
        hasher.finalize().into(),
    ))
}

#[cfg(feature = "composition-python")]
const fn python_extension_limits() -> ProductExtensionLimits {
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
fn validate_python_evidence(
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
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(crate::js_code_mode::RquickjsCodeModeIntegration {
            identity,
            profile: CodexIntegrationProfile::ExactReference,
            nested_catalog: Vec::new(),
            sidecar,
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
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(crate::js_code_mode::RquickjsCodeModeIntegration {
            identity,
            profile: CodexIntegrationProfile::MinimalCore,
            nested_catalog,
            sidecar,
        });
    let descriptor = neutral_minimal_descriptor(descriptor)
        .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor,
        integration,
        attachment,
    })
}

fn attach_product_javascript(
    builder: AgentBuilder,
    descriptor: &ProductJavaScriptCompositionDescriptorV1,
    host: PathBuf,
    identity: Vec<u8>,
    attachment: LocalCompositionAttachmentIdentity,
    evidence: ProductExtensionEvidence,
    presentation_kind: plakous_tools::CodingPresentation,
) -> Result<AgentBuilder, AgentError> {
    let (_, _, _, _, limits, _) = evidence.extension_facts();
    let max_output_tokens = usize::try_from(limits.max_output_tokens).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "product extension output limit exceeds the host integer boundary",
        )
    })?;
    let nested_names = descriptor
        .nested_tools()
        .iter()
        .map(|tool| tool.exposed_name().clone())
        .collect::<Vec<_>>();
    let neutral = neutral_product_descriptor(descriptor)
        .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(ProductJavaScriptIntegration {
            sidecar: host,
            identity,
            nested_names: nested_names.into(),
            presentation_kind,
            evidence,
            limits,
            max_output_tokens,
        });
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor: neutral,
        integration,
        attachment,
    })
}

#[cfg(feature = "composition-python")]
fn attach_python(
    builder: AgentBuilder,
    descriptor: &PythonPtcDescriptorV1,
    worker: PathBuf,
    identity: Vec<u8>,
    attachment: LocalCompositionAttachmentIdentity,
    evidence: ProductExtensionEvidence,
    presentation_kind: plakous_tools::CodingPresentation,
) -> Result<AgentBuilder, AgentError> {
    let nested_names = descriptor
        .nested_tools()
        .iter()
        .map(|tool| tool.canonical_name().clone())
        .collect::<Vec<_>>();
    let python_bindings = descriptor
        .nested_tools()
        .iter()
        .map(|tool| {
            (
                tool.python_name().to_owned(),
                crate::monty_python::PythonToolBinding {
                    tool_name: tool.canonical_name().as_str().to_owned(),
                    input_kind: if tool.string_input() {
                        crate::monty_python::PythonToolInput::String
                    } else {
                        crate::monty_python::PythonToolInput::Dictionary
                    },
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let neutral = neutral_python_descriptor(descriptor).map_err(invalid)?;
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(PythonIntegration {
        worker,
        identity,
        nested_names: nested_names.into(),
        python_bindings: Arc::new(python_bindings),
        presentation: descriptor.presentation(),
        presentation_kind,
        evidence,
    });
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor: neutral,
        integration,
        attachment,
    })
}

#[cfg(feature = "composition-python")]
struct PythonIntegration {
    worker: PathBuf,
    identity: Vec<u8>,
    nested_names: Arc<[ToolName]>,
    python_bindings: Arc<BTreeMap<String, crate::monty_python::PythonToolBinding>>,
    presentation: plakous_compose::PythonPtcPresentationV1,
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: ProductExtensionEvidence,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "composition-python")]
struct PythonStructuredRunInput {
    code: String,
    yield_time_ms: Option<f64>,
    max_output_bytes: Option<f64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "composition-python")]
struct PythonWaitInput {
    cell_id: String,
    yield_time_ms: Option<f64>,
    max_output_bytes: Option<f64>,
    terminate: Option<bool>,
}

#[cfg(feature = "composition-python")]
fn parse_python_wait(input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
    let input: PythonWaitInput =
        serde_json::from_value(input).map_err(|_| "invalid Python wait input".to_owned())?;
    if input.cell_id.is_empty() {
        return Err("Python wait cell_id must not be empty".into());
    }
    let yield_time_ms = input
        .yield_time_ms
        .map_or(Ok(PYTHON_DEFAULT_YIELD_TIME_MS), |value| {
            whole_u64(value, "yield_time_ms")
        })?
        .min(600_000);
    let max_output_bytes = input
        .max_output_bytes
        .map_or(Ok(PythonPtcLimitsV1::EXACT.final_result_bytes), |value| {
            whole_u64(value, "max_output_bytes")
        })?;
    if max_output_bytes == 0 || max_output_bytes > PythonPtcLimitsV1::EXACT.final_result_bytes {
        return Err("Python wait output byte bound exceeded".into());
    }
    Ok(LocalCompositionWaitInput {
        cell_id: input.cell_id,
        yield_time_ms,
        max_output_tokens: usize::try_from(max_output_bytes)
            .map_err(|_| "Python output bound exceeds usize".to_owned())?,
        terminate: input.terminate.unwrap_or(false),
    })
}

#[cfg(feature = "composition-python")]
impl LocalCompositionIntegration for PythonIntegration {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract::PYTHON
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::AgentIncarnationPersistent
    }
    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::BytesPythonPtcV1
    }
    fn environment_identity(&self) -> Vec<u8> {
        let mut value = self.identity.clone();
        value.extend_from_slice(&self.evidence.identity().identity_bytes());
        value
    }
    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, String> {
        let (source, max_output_bytes, yield_time_ms) = match self.presentation {
            plakous_compose::PythonPtcPresentationV1::StructuredFunctions => {
                let value: PythonStructuredRunInput = serde_json::from_value(input)
                    .map_err(|_| "invalid structured Python run input".to_owned())?;
                (
                    value.code,
                    value
                        .max_output_bytes
                        .map_or(Ok(PythonPtcLimitsV1::EXACT.final_result_bytes), |value| {
                            whole_u64(value, "max_output_bytes")
                        })?,
                    value
                        .yield_time_ms
                        .map_or(Ok(PYTHON_DEFAULT_YIELD_TIME_MS), |value| {
                            whole_u64(value, "yield_time_ms")
                        })?,
                )
            }
            plakous_compose::PythonPtcPresentationV1::CustomRunFunctionWait => {
                let source = input
                    .as_str()
                    .ok_or_else(|| "custom Python run input must be raw UTF-8 text".to_owned())?
                    .to_owned();
                (
                    source,
                    PythonPtcLimitsV1::EXACT.final_result_bytes,
                    PYTHON_DEFAULT_YIELD_TIME_MS,
                )
            }
        };
        if source.is_empty() || source.len() > PYTHON_SOURCE_MAX_BYTES {
            return Err("Python source must be non-empty and within 256 KiB".into());
        }
        if yield_time_ms > PYTHON_DEFAULT_YIELD_TIME_MS
            || max_output_bytes == 0
            || max_output_bytes > PythonPtcLimitsV1::EXACT.final_result_bytes
        {
            return Err("Python run input exceeds its exact byte bound".into());
        }
        Ok(LocalCompositionExecuteInput {
            source,
            yield_time_ms,
            max_output_tokens: usize::try_from(max_output_bytes)
                .map_err(|_| "Python output byte bound exceeds usize".to_owned())?,
        })
    }
    fn parse_wait(&self, input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        parse_python_wait(input)
    }
    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        Box::pin(async move {
            crate::monty_python::session_bundle(&self.worker, (*self.python_bindings).clone()).await
        })
    }
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }
    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        PythonPtcV1::compile_for_kind(
            job_config,
            registry,
            target,
            self.presentation,
            self.presentation_kind,
            self.nested_names.iter().cloned(),
        )
        .map_err(|error| error.to_string())
        .and_then(|value| neutral_python_descriptor(&value))
    }
    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        validate_python_configuration(
            job_config,
            descriptor,
            projection,
            registry,
            configuration,
            &self.nested_names,
            self.presentation,
            self.presentation_kind,
            &self.evidence,
        )
    }
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        maximum: usize,
    ) -> Vec<LocalCompositionContentItem> {
        truncate_python_items(items, maximum)
    }
    fn max_yield_time_ms(&self) -> u64 {
        PYTHON_DEFAULT_YIELD_TIME_MS
    }
    fn max_output_tokens(&self) -> usize {
        usize::try_from(PythonPtcLimitsV1::EXACT.final_result_bytes).unwrap_or(usize::MAX)
    }
    fn max_outstanding_nested_calls(&self) -> usize {
        PythonPtcLimitsV1::EXACT.concurrent_nested_calls as usize
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CodexIntegrationProfile {
    ExactReference,
    MinimalCore,
}

struct ProductJavaScriptIntegration {
    sidecar: PathBuf,
    identity: Vec<u8>,
    nested_names: Arc<[ToolName]>,
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: ProductExtensionEvidence,
    limits: ProductExtensionLimits,
    max_output_tokens: usize,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductRunWireInput {
    source: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<u64>,
}

impl LocalCompositionIntegration for ProductJavaScriptIntegration {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract::JAVASCRIPT
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::RootTurnJoined
    }

    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::TokensScriptV1
    }

    fn environment_identity(&self) -> Vec<u8> {
        let mut identity = self.identity.clone();
        identity.extend_from_slice(&self.evidence.identity().identity_bytes());
        identity
    }

    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, String> {
        let input: ProductRunWireInput = serde_json::from_value(input)
            .map_err(|_| "invalid product JavaScript run input".to_owned())?;
        if input.source.is_empty()
            || u64::try_from(input.source.len())
                .map_or(true, |length| length > self.limits.max_source_bytes)
        {
            return Err(
                "product JavaScript source must be non-empty and within its UTF-8 byte limit"
                    .to_owned(),
            );
        }
        let yield_time_ms = input
            .yield_time_ms
            .unwrap_or(PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS);
        let max_output_tokens = input
            .max_output_tokens
            .unwrap_or(PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS);
        if yield_time_ms > self.limits.max_yield_time_ms
            || max_output_tokens == 0
            || max_output_tokens > self.limits.max_output_tokens
        {
            return Err(
                "product JavaScript run input exceeds its evidence-bound limits".to_owned(),
            );
        }
        Ok(LocalCompositionExecuteInput {
            source: input.source,
            yield_time_ms,
            max_output_tokens: usize::try_from(max_output_tokens)
                .map_err(|_| "product JavaScript output limit exceeds usize".to_owned())?,
        })
    }

    fn parse_wait(&self, input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        let input: WaitWireInput = serde_json::from_value(input)
            .map_err(|_| "invalid product JavaScript wait input".to_owned())?;
        if input.cell_id.is_empty() {
            return Err("product JavaScript wait cell_id must not be empty".to_owned());
        }
        let yield_time_ms = input
            .yield_time_ms
            .map_or(Ok(PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS), |value| {
                whole_u64(value, "yield-time_ms")
            })?
            .min(600_000);
        let default_output_tokens = usize::try_from(PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS)
            .map_err(|_| "product JavaScript default output limit exceeds usize".to_owned())?;
        let max_output_tokens = input
            .max_tokens
            .map_or(Ok(default_output_tokens), |value| {
                whole_u64(value, "max_tokens").and_then(|value| {
                    usize::try_from(value)
                        .map_err(|_| "product JavaScript wait max_tokens exceeds usize".to_owned())
                })
            })?;
        if max_output_tokens == 0 || max_output_tokens > self.max_output_tokens {
            return Err(
                "product JavaScript wait input exceeds its evidence-bound limits".to_owned(),
            );
        }
        Ok(LocalCompositionWaitInput {
            cell_id: input.cell_id,
            yield_time_ms,
            max_output_tokens,
            terminate: input.terminate.unwrap_or(false),
        })
    }

    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        let sidecar = self.sidecar.clone();
        let identity = self.identity.clone();
        Box::pin(async move {
            verify_sidecar_before_spawn(&sidecar, &identity)?;
            let host = plakous_js_eval::SidecarHost::spawn(&sidecar)?;
            Ok(crate::js_code_mode::session_bundle_from_engine(Arc::new(
                host,
            )))
        })
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async move { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }

    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        ProductJavaScriptCompositionV1::compile_for_kind(
            job_config,
            registry,
            target,
            self.presentation_kind,
            self.nested_names.iter().cloned(),
        )
        .map_err(|error| error.to_string())
        .and_then(|descriptor| neutral_product_descriptor(&descriptor))
    }

    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        validate_product_configuration(
            job_config,
            descriptor,
            projection,
            registry,
            configuration,
            &self.nested_names,
            self.presentation_kind,
            &self.evidence,
        )
    }

    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem> {
        native::truncate_content_items(
            items.into_iter().map(to_native_item).collect(),
            max_output_tokens,
        )
        .into_iter()
        .map(from_native_item)
        .collect()
    }

    fn max_yield_time_ms(&self) -> u64 {
        self.limits.max_yield_time_ms
    }

    fn max_output_tokens(&self) -> usize {
        self.max_output_tokens
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "process budgets are an extra exact construction identity, not a new concern"
)]
fn validate_product_configuration(
    job_config: plakous_tools::JobConfig,
    descriptor: &LocalCompositionDescriptor,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    nested_names: &[ToolName],
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: &ProductExtensionEvidence,
) -> Result<(), String> {
    if !projection.is_for_registry(&registry)
        || !descriptor.outer_projection.is_for_registry(&registry)
        || !descriptor.nested_projection.is_for_registry(&registry)
    {
        return Err("product JavaScript projection belongs to a foreign registry".into());
    }
    let effective = configuration.effective();
    let model = effective.descriptor();
    let family = model
        .model_family()
        .ok_or_else(|| "product JavaScript target has no exact model family".to_owned())?;
    let expected_target = ProviderModelProjectionIdentityV2::new(
        model.integration_class(),
        model.protocol().as_str(),
        model.route().as_str(),
        model.profile().as_str(),
        family.as_str(),
        model.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|error| error.to_string())?;
    if !matches!(
        expected_target.integration_class(),
        IntegrationClass::Standard | IntegrationClass::Custom
    ) || descriptor.outer_projection.target() != &expected_target
        || descriptor.nested_projection.target() != &expected_target
        || projection.target() != &expected_target
    {
        return Err(
            "product JavaScript descriptor is foreign to the exact provider configuration".into(),
        );
    }
    if evidence.configuration_identities()
        != (
            configuration.selected_identity(),
            configuration.effective_identity(),
        )
        || evidence.target() != expected_target.integration_class()
    {
        return Err(
            "product extension evidence is foreign to the exact provider configuration".into(),
        );
    }
    let configured = &effective.behavior().tools;
    if configured.len() != 2
        || configured.iter().any(|tool| {
            tool.execution() != ToolExecutionDisposition::Local
                || !matches!(
                    tool.definition().name().as_str(),
                    PRODUCT_JAVASCRIPT_RUN_NAME_V1 | PRODUCT_JAVASCRIPT_WAIT_NAME_V1
                )
                || registry.definition(tool.definition().name()) != Some(tool.definition())
        })
        || !configured
            .iter()
            .any(|tool| tool.definition().name().as_str() == PRODUCT_JAVASCRIPT_RUN_NAME_V1)
        || !configured
            .iter()
            .any(|tool| tool.definition().name().as_str() == PRODUCT_JAVASCRIPT_WAIT_NAME_V1)
    {
        return Err(
            "provider configuration does not own the exact product JavaScript markers".into(),
        );
    }
    let exact = ProductJavaScriptCompositionV1::compile_for_kind(
        job_config,
        registry,
        expected_target,
        presentation_kind,
        nested_names.iter().cloned(),
    )
    .map_err(|error| error.to_string())?;
    let neutral = neutral_product_descriptor(&exact)?;
    let (_, _, catalog, presentation, _, _) = evidence.extension_facts();
    if neutral.identity != descriptor.identity
        || exact.outer_projection().digest() != projection.digest()
        || catalog != exact.nested_projection().digest()
        || presentation != exact.outer_projection().digest()
    {
        return Err("product JavaScript descriptor, receipt, or outer projection drifted".into());
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "nested Direct pack is an extra exact identity, not a new concern"
)]
#[cfg(feature = "composition-python")]
fn validate_python_configuration(
    job_config: plakous_tools::JobConfig,
    descriptor: &LocalCompositionDescriptor,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    nested_names: &[ToolName],
    presentation: plakous_compose::PythonPtcPresentationV1,
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: &ProductExtensionEvidence,
) -> Result<(), String> {
    if !projection.is_for_registry(&registry)
        || !descriptor.outer_projection.is_for_registry(&registry)
        || !descriptor.nested_projection.is_for_registry(&registry)
    {
        return Err("Python projection belongs to a foreign registry".into());
    }
    let effective = configuration.effective();
    let model = effective.descriptor();
    let family = model
        .model_family()
        .ok_or_else(|| "Python target has no exact model family".to_owned())?;
    let target = ProviderModelProjectionIdentityV2::new(
        model.integration_class(),
        model.protocol().as_str(),
        model.route().as_str(),
        model.profile().as_str(),
        family.as_str(),
        model.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|error| error.to_string())?;
    if !matches!(
        target.integration_class(),
        IntegrationClass::Standard | IntegrationClass::Custom
    ) || descriptor.outer_projection.target() != &target
        || descriptor.nested_projection.target() != &target
        || projection.target() != &target
        || evidence.configuration_identities()
            != (
                configuration.selected_identity(),
                configuration.effective_identity(),
            )
        || evidence.target() != target.integration_class()
    {
        return Err(
            "Python descriptor or evidence is foreign to the exact provider configuration".into(),
        );
    }
    let exact = PythonPtcV1::compile_for_kind(
        job_config,
        registry,
        target,
        presentation,
        presentation_kind,
        nested_names.iter().cloned(),
    )
    .map_err(|error| error.to_string())?;
    let neutral = neutral_python_descriptor(&exact)?;
    let (_, _, catalog, presentation_fact, limits, _) = evidence.extension_facts();
    if neutral.identity != descriptor.identity
        || exact.outer_projection().digest() != projection.digest()
        || catalog != exact.catalog_identity()
        || presentation_fact != exact.presentation_identity()
        || limits != python_extension_limits()
    {
        return Err("Python descriptor, receipt, or outer projection drifted".into());
    }
    Ok(())
}

#[cfg(feature = "composition-python")]
fn truncate_python_items(
    items: Vec<LocalCompositionContentItem>,
    maximum: usize,
) -> Vec<LocalCompositionContentItem> {
    let mut remaining = maximum;
    let mut output = Vec::new();
    for item in items {
        if remaining == 0 {
            break;
        }
        if let LocalCompositionContentItem::Text(mut text) = item {
            if text.len() > remaining {
                let mut boundary = remaining;
                while boundary > 0 && !text.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text.truncate(boundary);
            }
            remaining = remaining.saturating_sub(text.len());
            if !text.is_empty() {
                output.push(LocalCompositionContentItem::Text(text));
            }
        }
    }
    output
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitWireInput {
    pub(crate) cell_id: String,
    pub(crate) yield_time_ms: Option<f64>,
    pub(crate) max_tokens: Option<f64>,
    pub(crate) terminate: Option<bool>,
}

pub(crate) fn whole_u64(value: f64, field: &str) -> Result<u64, String> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > 9_007_199_254_740_991.0
    {
        return Err(format!(
            "Code Mode wait {field} must be a non-negative whole number"
        ));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "range checked above"
    )]
    Ok(value as u64)
}

pub(crate) fn neutral_descriptor(
    exact: &CodexNativeLocalCodeModeDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.exposed_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: "exec".into(),
        wait_name: "wait".into(),
        marker_executor: ToolExecutorId::new("plakous.codex-code-mode.driver-marker.v1")
            .map_err(|error| error.to_string())?,
    })
}

pub(crate) fn neutral_minimal_descriptor(
    exact: &CodexMinimalLocalCodeModeDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.exposed_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: "exec".into(),
        wait_name: "wait".into(),
        marker_executor: ToolExecutorId::new(
            plakous_tools::CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1,
        )
        .map_err(|error| error.to_string())?,
    })
}

fn neutral_product_descriptor(
    exact: &ProductJavaScriptCompositionDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.exposed_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: PRODUCT_JAVASCRIPT_RUN_NAME_V1.into(),
        wait_name: PRODUCT_JAVASCRIPT_WAIT_NAME_V1.into(),
        marker_executor: ToolExecutorId::new(PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1)
            .map_err(|error| error.to_string())?,
    })
}

#[cfg(feature = "composition-python")]
fn neutral_python_descriptor(
    exact: &PythonPtcDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.python_name().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: PYTHON_PTC_RUN_NAME_V1.into(),
        wait_name: PYTHON_PTC_WAIT_NAME_V1.into(),
        marker_executor: ToolExecutorId::new(PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1)
            .map_err(|error| error.to_string())?,
    })
}

pub(crate) fn from_native_item(value: native::ContentItem) -> LocalCompositionContentItem {
    match value {
        native::ContentItem::InputText { text } => LocalCompositionContentItem::Text(text),
        native::ContentItem::InputImage { image_url, detail } => {
            LocalCompositionContentItem::Image {
                url: image_url,
                detail: detail.map(from_native_detail),
            }
        }
        native::ContentItem::InputAudio { audio_url } => {
            LocalCompositionContentItem::Audio(audio_url)
        }
    }
}
fn from_native_detail(value: native::ImageDetail) -> LocalCompositionImageDetail {
    match value {
        native::ImageDetail::Auto => LocalCompositionImageDetail::Auto,
        native::ImageDetail::Low => LocalCompositionImageDetail::Low,
        native::ImageDetail::High => LocalCompositionImageDetail::High,
        native::ImageDetail::Original => LocalCompositionImageDetail::Original,
    }
}
pub(crate) fn to_native_item(value: LocalCompositionContentItem) -> native::ContentItem {
    match value {
        LocalCompositionContentItem::Text(text) => native::ContentItem::InputText { text },
        LocalCompositionContentItem::Image { url, detail } => native::ContentItem::InputImage {
            image_url: url,
            detail: detail.map(|value| match value {
                LocalCompositionImageDetail::Auto => native::ImageDetail::Auto,
                LocalCompositionImageDetail::Low => native::ImageDetail::Low,
                LocalCompositionImageDetail::High => native::ImageDetail::High,
                LocalCompositionImageDetail::Original => native::ImageDetail::Original,
            }),
        },
        LocalCompositionContentItem::Audio(audio_url) => {
            native::ContentItem::InputAudio { audio_url }
        }
    }
}

#[cfg(test)]
mod artifact_recovery_tests {
    #[tokio::test]
    async fn sidecar_spawn_refuses_bytes_changed_after_attachment()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("host");
        std::fs::write(&path, b"attached sidecar candidate")?;
        let path = path.canonicalize()?;
        let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&path)?;
        let identity = match artifact.kind {
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                environment_identity,
                ..
            } => environment_identity,
            #[cfg(feature = "composition-python")]
            LocalCompositionEngineArtifactKind::MontyPythonPtcV1 { .. } => {
                unreachable!("constructor always produces a JavaScript artifact")
            }
        };
        let integration = crate::js_code_mode::RquickjsCodeModeIntegration {
            identity,
            profile: CodexIntegrationProfile::MinimalCore,
            nested_catalog: Vec::new(),
            sidecar: path.clone(),
        };
        std::fs::write(path, b"changed sidecar bytes")?;
        let error = integration
            .create_session(1, 10_000, 10_000)
            .await
            .err()
            .ok_or("changed sidecar was spawned")?;
        assert!(error.contains("changed after attachment"), "{error}");
        Ok(())
    }
    #[test]
    fn sidecar_artifact_rejects_relative_paths_and_accepts_canonical_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let relative = Path::new("Cargo.toml");
        let error = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(relative)
            .err()
            .ok_or("relative artifact path was accepted")?;
        assert!(error.to_string().contains("path must already be canonical"));
        let canonical = relative.canonicalize()?;
        let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&canonical)?;
        assert_eq!(artifact.path(), canonical);
        Ok(())
    }
    use super::*;
    use plakous_agent::internal::facade_support::StoredCompositionArtifact;

    fn recorded(
        artifact: &LocalCompositionEngineArtifact,
    ) -> Result<StoredCompositionArtifact, AgentError> {
        let host = match &artifact.kind {
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                environment_identity,
                ..
            } => environment_identity.clone(),
            #[cfg(feature = "composition-python")]
            LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
                environment_identity,
                ..
            } => environment_identity.clone(),
        };
        Ok(StoredCompositionArtifact {
            profile_identity: artifact.engine_contract_identity().identity_bytes(),
            artifact_identity: artifact.identity().identity_bytes(),
            binding_identity: attachment_binding_identity(
                artifact.engine_contract_identity(),
                artifact.identity(),
                artifact.engine_contract_identity(),
                None,
            )?
            .identity_bytes(),
            engine_contract_identity: artifact.engine_contract_identity().identity_bytes(),
            host,
        })
    }

    #[test]
    fn recorded_artifacts_refuse_forged_hash_contract_and_noncanonical_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("host");
        std::fs::write(&path, b"offline artifact")?;
        let path = path.canonicalize()?;
        let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&path)?;
        let mut exact = recorded(&artifact)?;
        assert_eq!(
            restore_stored_artifact(&exact)?.identity(),
            artifact.identity()
        );
        // Product PTC's evidence suffix is outside the artifact preimage.
        exact
            .host
            .extend_from_slice(&artifact.engine_contract_identity().identity_bytes());
        exact.binding_identity = attachment_binding_identity(
            artifact.engine_contract_identity(),
            artifact.identity(),
            artifact.engine_contract_identity(),
            Some(artifact.engine_contract_identity()),
        )?
        .identity_bytes();
        assert_eq!(
            restore_stored_artifact(&exact)?.identity(),
            artifact.identity()
        );
        exact.engine_contract_identity[0] ^= 1;
        assert!(restore_stored_artifact(&exact).is_err());
        let mut wrong_binding = recorded(&artifact)?;
        wrong_binding.binding_identity[0] ^= 1;
        assert!(restore_stored_artifact(&wrong_binding).is_err());
        let mut forged = recorded(&artifact)?;
        *forged.host.last_mut().ok_or("empty host")? ^= 1;
        assert!(restore_stored_artifact(&forged).is_err());
        let alias = path.parent().ok_or("parent")?.join(".").join("host");
        let original = recorded(&artifact)?;
        let hash = &original.host[original.host.len() - 32..];
        let host = [
            b"rquickjs-host".as_slice(),
            alias.as_os_str().as_encoded_bytes(),
            hash,
        ]
        .concat();
        let noncanonical_artifact =
            digest_environment(b"plakous.local-composition.engine-artifact.v1", &host)?;
        let noncanonical = StoredCompositionArtifact {
            profile_identity: artifact.engine_contract_identity().identity_bytes(),
            artifact_identity: noncanonical_artifact.identity_bytes(),
            binding_identity: attachment_binding_identity(
                artifact.engine_contract_identity(),
                noncanonical_artifact,
                artifact.engine_contract_identity(),
                None,
            )?
            .identity_bytes(),
            engine_contract_identity: artifact.engine_contract_identity().identity_bytes(),
            host,
        };
        assert!(restore_stored_artifact(&noncanonical).is_err());
        #[cfg(feature = "composition-python")]
        {
            let python = LocalCompositionEngineArtifact::monty_pool_v1(&path)?;
            assert_eq!(
                restore_stored_artifact(&recorded(&python)?)?.identity(),
                python.identity()
            );
            let mut disagreement = recorded(&python)?;
            disagreement.engine_contract_identity =
                artifact.engine_contract_identity().identity_bytes();
            disagreement.binding_identity = attachment_binding_identity(
                python.engine_contract_identity(),
                python.identity(),
                artifact.engine_contract_identity(),
                None,
            )?
            .identity_bytes();
            assert!(restore_stored_artifact(&disagreement).is_err());
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "composition-python"))]
mod monty_upgrade_tests {
    use super::*;

    /// The engine contract this build's predecessor recorded: Monty 1.1.0
    /// under adapter revision v6.
    fn previous_monty_contract() -> Result<Digest, AgentError> {
        Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.monty-python-engine-contract.v2",
                b"monty-pool",
                b"1.1.0",
                b"plakous.monty-python-adapter.v6",
            ],
        )
        .map_err(identity_error)
    }

    #[test]
    fn the_contract_names_monty_1_2_0_and_adapter_v7() -> Result<(), AgentError> {
        assert_eq!(
            crate::monty_python::ADAPTER_REVISION,
            "plakous.monty-python-adapter.v7"
        );
        let expected = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.monty-python-engine-contract.v2",
                b"monty-pool",
                b"1.2.0",
                b"plakous.monty-python-adapter.v7",
            ],
        )
        .map_err(identity_error)?;
        assert_eq!(monty_engine_contract_identity()?, expected);
        Ok(())
    }

    #[test]
    fn a_py_ptc_record_under_the_previous_contract_is_a_typed_refusal()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("monty");
        std::fs::write(&path, b"offline worker")?;
        let artifact = LocalCompositionEngineArtifact::monty_pool_v1(path.canonicalize()?)?;
        let LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
            environment_identity,
            ..
        } = &artifact.kind
        else {
            return Err("monty artifact".into());
        };
        let previous = previous_monty_contract()?;
        assert_ne!(previous, artifact.engine_contract_identity());
        // A self-consistent record of the same worker bytes, bound under the
        // previous contract: only the contract disagrees with this build.
        let stored = plakous_agent::internal::facade_support::StoredCompositionArtifact {
            profile_identity: previous.identity_bytes(),
            artifact_identity: artifact.identity().identity_bytes(),
            binding_identity: attachment_binding_identity(
                previous,
                artifact.identity(),
                previous,
                None,
            )?
            .identity_bytes(),
            engine_contract_identity: previous.identity_bytes(),
            host: environment_identity.clone(),
        };
        let Err(refusal) = restore_stored_artifact(&stored) else {
            return Err("a previous-contract record must not reopen".into());
        };
        assert_eq!(refusal.kind(), AgentErrorKind::IncompatibleSession);
        assert!(!refusal.message().contains("plak session"));
        assert!(
            refusal.message().contains("no inference was dispatched"),
            "{}",
            refusal.message()
        );
        Ok(())
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::{
        Digest, DigestDomain, LocalCompositionContentItem, LocalCompositionEngineArtifact,
        ProductExtensionLimits, PythonStructuredRunInput, attachment_binding_identity,
        monty_engine_contract_identity, product_javascript_limits_are_exact,
        python_extension_limits, rquickjs_sidecar_engine_contract_identity, truncate_python_items,
        validate_engine_contract_binding,
    };

    #[test]
    fn engine_contract_binding_accepts_exact_identity_and_rejects_neighbors() {
        let Ok(exact) = rquickjs_sidecar_engine_contract_identity() else {
            panic!("exact engine identity must construct");
        };
        assert!(validate_engine_contract_binding(exact, exact).is_ok());

        let Ok(neighbor) = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[b"plakous.local-composition.unqualified-python-neighbor.v1"],
        ) else {
            panic!("neighbor identity must construct");
        };
        let Err(rejected) = validate_engine_contract_binding(exact, neighbor) else {
            panic!("a neighboring engine must not inherit qualification");
        };
        assert!(
            rejected
                .message()
                .contains("engine artifact identities do not match")
        );
    }

    #[test]
    fn python_and_javascript_engine_contracts_are_closed_cross_pairs()
    -> Result<(), Box<dyn std::error::Error>> {
        let codex = rquickjs_sidecar_engine_contract_identity()?;
        let monty = monty_engine_contract_identity()?;
        assert_ne!(codex, monty);
        assert!(validate_engine_contract_binding(monty, monty).is_ok());
        assert!(validate_engine_contract_binding(monty, codex).is_err());
        assert!(validate_engine_contract_binding(codex, monty).is_err());
        Ok(())
    }

    #[test]
    fn monty_artifact_reconstructs_from_exact_held_paths() -> Result<(), Box<dyn std::error::Error>>
    {
        // System true/false may be aliases of the same BusyBox object. Use
        // distinct held files so this test actually exercises artifact drift.
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let path = root.join("worker");
        std::fs::write(&path, b"original worker bytes")?;
        let artifact = LocalCompositionEngineArtifact::monty_pool_v1(&path)?;
        assert_eq!(artifact.path(), path);
        assert_eq!(
            artifact.engine_contract_identity(),
            monty_engine_contract_identity()?
        );
        assert_ne!(artifact.identity(), artifact.host_identity());

        let drifted_path = root.join("other-worker");
        std::fs::write(&drifted_path, b"different worker bytes")?;
        let drifted = LocalCompositionEngineArtifact::monty_pool_v1(&drifted_path)?;
        assert_ne!(artifact.identity(), drifted.identity());
        assert_ne!(artifact.containment(), drifted.containment());

        // Content replacement at the same canonical path is also identity drift.
        std::fs::write(&path, b"replacement worker bytes")?;
        let replaced = LocalCompositionEngineArtifact::monty_pool_v1(&path)?;
        assert_ne!(artifact.identity(), replaced.identity());
        assert_ne!(artifact.containment(), replaced.containment());
        Ok(())
    }

    #[test]
    fn compiled_python_descriptor_declares_wait_py() -> Result<(), Box<dyn std::error::Error>> {
        let mut registry = plakous_tools::ToolRegistryBuilder::default();
        plakous_tools::register_packs(
            &plakous_compose::standard_coding_packs_in_memory()?,
            &mut registry,
        )?;
        super::PythonPtcV1::register_markers_into(&mut registry)?;
        let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"python-descriptor-test"],
        )?)?)?;
        let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Custom,
            "custom.responses",
            "test.route",
            "test.python",
            "responses-family",
            "test.model",
            "test.model",
            configuration,
            1,
        )?;
        let exact = super::PythonPtcV1::compile(
            registry.freeze(),
            target,
            plakous_compose::PythonPtcPresentationV1::StructuredFunctions,
            [plakous_model::ToolName::new("read_file")?],
        )?;
        let compiled = super::neutral_python_descriptor(&exact)?;
        assert_eq!(compiled.execute_name.as_ref(), "exec_py");
        assert_eq!(compiled.wait_name.as_ref(), "wait_py");
        Ok(())
    }

    #[test]
    fn python_limits_and_utf8_byte_truncation_are_exact() {
        assert_eq!(
            python_extension_limits(),
            ProductExtensionLimits {
                max_source_bytes: 262_144,
                max_yield_time_ms: 10_000,
                max_output_tokens: 1_048_576,
            }
        );
        let output = truncate_python_items(
            vec![
                LocalCompositionContentItem::Text("aéz".into()),
                LocalCompositionContentItem::Audio("discarded".into()),
            ],
            2,
        );
        assert_eq!(output, vec![LocalCompositionContentItem::Text("a".into())]);
    }

    #[test]
    fn python_structured_inputs_accept_code_and_byte_bound() {
        let Ok(run): Result<PythonStructuredRunInput, _> =
            serde_json::from_value(serde_json::json!({
                "code": "1 + 1",
                "max_output_bytes": 20_000
            }))
        else {
            panic!("the published run schema must deserialize");
        };
        assert_eq!(run.code, "1 + 1");
        assert_eq!(run.max_output_bytes, Some(20_000.0));
        assert!(
            serde_json::from_value::<PythonStructuredRunInput>(serde_json::json!({
                "code": "1 + 1",
                "yield_time_ms": 9
            }))
            .is_ok(),
            "yield_time_ms is a Python run field"
        );
    }

    #[test]
    fn python_wait_defaults_clamps_and_rejects_invalid_bounds() -> Result<(), String> {
        let wait = super::parse_python_wait(serde_json::json!({"cell_id": "2"}))?;
        assert_eq!(wait.yield_time_ms, 10_000);
        assert_eq!(wait.max_output_tokens, 1_048_576);
        assert!(!wait.terminate);
        let wait = super::parse_python_wait(
            serde_json::json!({"cell_id": "2", "yield_time_ms": 900_000, "max_output_bytes": 20, "terminate": true}),
        )?;
        assert_eq!(wait.yield_time_ms, 600_000);
        assert_eq!(wait.max_output_tokens, 20);
        assert!(wait.terminate);
        for input in [
            serde_json::json!({"cell_id": ""}),
            serde_json::json!({"cell_id": "2", "yield_time_ms": -1}),
            serde_json::json!({"cell_id": "2", "yield_time_ms": 1.5}),
            serde_json::json!({"cell_id": "2", "max_output_bytes": 0}),
            serde_json::json!({"cell_id": "2", "max_output_bytes": 1_048_577}),
        ] {
            assert!(super::parse_python_wait(input).is_err());
        }
        Ok(())
    }

    #[test]
    fn product_javascript_limits_admit_only_the_exact_v1_tuple() {
        let exact = ProductExtensionLimits {
            max_source_bytes: 1_048_576,
            max_yield_time_ms: 10_000,
            max_output_tokens: 10_000,
        };
        assert!(product_javascript_limits_are_exact(exact));
        for neighbor in [
            ProductExtensionLimits {
                max_source_bytes: 0,
                ..exact
            },
            ProductExtensionLimits {
                max_source_bytes: 1_048_577,
                ..exact
            },
            ProductExtensionLimits {
                max_yield_time_ms: 9_999,
                ..exact
            },
            ProductExtensionLimits {
                max_output_tokens: 10_001,
                ..exact
            },
        ] {
            assert!(!product_javascript_limits_are_exact(neighbor));
        }
    }

    #[test]
    fn monty_coroutine_contract_rejects_the_eager_adapter_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let eager = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.monty-python-engine-contract.v1",
                b"monty-pool",
                b"0.0.21",
            ],
        )?;
        let current = monty_engine_contract_identity()?;
        assert_ne!(current, eager);
        assert!(validate_engine_contract_binding(current, eager).is_err());
        assert!(validate_engine_contract_binding(current, current).is_ok());
        Ok(())
    }

    #[test]
    fn native_attachment_identity_bytes_stay_exact_and_product_receipt_rotates_them()
    -> Result<(), Box<dyn std::error::Error>> {
        let profile = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"profile"])?;
        let artifact = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"artifact"])?;
        let engine = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"engine"])?;
        let receipt = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"receipt"])?;
        let expected_native = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.local-composition.attachment.v1",
                &profile.identity_bytes(),
                &artifact.identity_bytes(),
                &engine.identity_bytes(),
            ],
        )?;
        assert_eq!(
            attachment_binding_identity(profile, artifact, engine, None)?,
            expected_native
        );
        assert_ne!(
            attachment_binding_identity(profile, artifact, engine, Some(receipt))?,
            expected_native
        );
        Ok(())
    }
}
