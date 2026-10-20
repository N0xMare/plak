//! Attachment of the experimental neutral Python SDK (`py-harness-sdk`).
//!
//! The harness compiler owns the descriptor and the engine owns the cell; this
//! module binds the two for one agent incarnation: the neutral driver
//! descriptor (`run_py` and no wait), the engine program, the incarnation's
//! named state, and the descriptor authority that recompiles and validates the
//! exact descriptor on rebuild.

use std::{path::PathBuf, sync::Arc};

use plakous_agent::internal::{
    AgentBuilder, AgentError, InMemoryNamedState, LocalCompositionAttachmentIdentity,
    LocalCompositionConstruction, LocalCompositionDescriptor, LocalCompositionIntegration,
    LocalCompositionToolDefinition, NamedStateLimits,
};
use plakous_compose::{
    PythonHarnessSdkAccessV1, PythonHarnessSdkDescriptorV1, PythonHarnessSdkOpOrderV1,
    PythonHarnessSdkV1,
};
use plakous_model::{
    CompiledProviderConfiguration, Digest, DigestDomain, IntegrationClass, ToolExecutionDisposition,
};
use plakous_tools::{
    CodingPresentation, FrozenToolRegistry, JobConfig,
    PYTHON_HARNESS_SDK_DRIVER_MARKER_EXECUTOR_ID_V1, PYTHON_HARNESS_SDK_RUN_NAME_V1,
    ProviderModelProjectionIdentityV2, PythonHarnessSdkLimitsV1, ToolExecutorId, ToolProjectionMap,
};

use super::{
    LocalCompositionEngineArtifact, LocalCompositionProfile, ProductExtensionEvidence,
    ProductExtensionLimits, identity_error, invalid,
};
use crate::monty_python::{
    HARNESS_SDK_ENGINE_REVISION, HarnessSdkDescriptorAuthority, HarnessSdkIntegration,
    HarnessSdkProgram,
};

/// The SDK engine contract: the Monty worker driven by the SDK engine.
pub(super) fn engine_contract_identity() -> Result<Digest, AgentError> {
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[
            b"plakous.monty-python-harness-sdk-engine-contract.v1",
            b"monty-pool",
            b"1.2.0",
            HARNESS_SDK_ENGINE_REVISION.as_bytes(),
        ],
    )
    .map_err(identity_error)
}

/// The limits a receipt binds: source bytes, the foreground budget as the
/// yield bound (a cell never yields before it), and model-visible output bytes.
pub(super) const fn extension_limits() -> ProductExtensionLimits {
    let limits = PythonHarnessSdkLimitsV1::EXACT;
    ProductExtensionLimits {
        max_source_bytes: limits.source_bytes,
        max_yield_time_ms: limits.foreground_budget_ms,
        max_output_tokens: limits.output_bytes,
    }
}

/// Proves the receipt and the vertical bind exactly this profile and artifact.
pub(super) fn validate_attachment(
    profile: &LocalCompositionProfile,
    artifact: &LocalCompositionEngineArtifact,
    descriptor: &PythonHarnessSdkDescriptorV1,
    evidence: &ProductExtensionEvidence,
    presentation_kind: Option<CodingPresentation>,
) -> Result<(), AgentError> {
    let (profile_fact, artifact_fact, catalog, presentation, limits, host) =
        evidence.extension_facts();
    let (engine_contract, containment) = evidence.engine_identities();
    if evidence.target() != descriptor.outer_projection().target().integration_class()
        || profile_fact != profile.identity()
        || artifact_fact != artifact.identity()
        || catalog != descriptor.catalog_identity()
        || presentation != descriptor.outer_projection().digest()
        || limits != extension_limits()
        || host != artifact.host_identity()
        || engine_contract != artifact.engine_contract_identity()
        || containment != artifact.containment().identity()
    {
        return Err(invalid(
            "Python SDK extension evidence disagrees with the exact profile attachment",
        ));
    }
    if presentation_kind.is_some_and(|kind| {
        PythonHarnessSdkOpOrderV1::for_presentation(kind) != descriptor.op_order()
    }) {
        return Err(invalid(
            "Python SDK descriptor was compiled for a different coding presentation",
        ));
    }
    Ok(())
}

/// The engine program compiled with the descriptor.
fn program(descriptor: &PythonHarnessSdkDescriptorV1) -> HarnessSdkProgram {
    HarnessSdkProgram {
        stubs: descriptor.stubs().to_owned(),
        bootstrap: descriptor.bootstrap().to_owned(),
        host_functions: descriptor
            .host_functions()
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        limits: descriptor.limits(),
    }
}

/// Attaches one agent incarnation's SDK composition to `builder`.
pub(super) fn attach(
    builder: AgentBuilder,
    descriptor: &PythonHarnessSdkDescriptorV1,
    worker: PathBuf,
    identity: Vec<u8>,
    attachment: LocalCompositionAttachmentIdentity,
    evidence: ProductExtensionEvidence,
    presentation_kind: CodingPresentation,
) -> Result<AgentBuilder, AgentError> {
    attach_program(
        builder,
        descriptor,
        program(descriptor),
        worker,
        identity,
        attachment,
        evidence,
        presentation_kind,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the engine program is the one fact a test substitutes"
)]
fn attach_program(
    builder: AgentBuilder,
    descriptor: &PythonHarnessSdkDescriptorV1,
    program: HarnessSdkProgram,
    worker: PathBuf,
    identity: Vec<u8>,
    attachment: LocalCompositionAttachmentIdentity,
    evidence: ProductExtensionEvidence,
    presentation_kind: CodingPresentation,
) -> Result<AgentBuilder, AgentError> {
    let neutral = neutral_descriptor(descriptor).map_err(invalid)?;
    let mut environment_identity = identity;
    environment_identity.extend_from_slice(&evidence.identity().identity_bytes());
    let state = Arc::new(InMemoryNamedState::new(NamedStateLimits::from_limits(
        &descriptor.limits(),
    )));
    let authority = Arc::new(Authority {
        access: descriptor.access(),
        presentation_kind,
        evidence,
    });
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(HarnessSdkIntegration::new(
        worker,
        program,
        environment_identity,
        authority,
        state,
    ));
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor: neutral,
        integration,
        attachment,
    })
}

/// Attaches `attachment` with a substituted engine program, so a test can
/// stand in for an engine that issues calls its descriptor does not admit.
#[cfg(test)]
pub(crate) fn attach_with_program(
    builder: AgentBuilder,
    attachment: super::LocalCompositionAttachment,
    program: HarnessSdkProgram,
) -> Result<AgentBuilder, AgentError> {
    let super::LocalCompositionAttachment {
        profile,
        artifact,
        binding_identity,
        evidence_receipt,
        evidence,
        presentation_kind,
    } = attachment;
    let (
        super::LocalCompositionProfileKind::PythonHarnessSdkV1(descriptor),
        super::LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
            path,
            environment_identity,
        },
        Some(evidence),
        Some(presentation_kind),
    ) = (profile.kind, artifact.kind, evidence, presentation_kind)
    else {
        return Err(invalid("not a Python SDK attachment"));
    };
    attach_program(
        builder,
        &descriptor,
        program,
        path,
        environment_identity,
        LocalCompositionAttachmentIdentity {
            profile: profile.profile_identity,
            artifact: artifact.artifact_identity,
            binding: binding_identity,
            engine_contract: profile.engine_contract_identity,
            evidence_receipt,
        },
        evidence,
        presentation_kind,
    )
}

/// The neutral driver descriptor: `run_py` executes and there is no wait.
fn neutral_descriptor(
    exact: &PythonHarnessSdkDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.canonical_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        helper_budget: Some(exact.limits().helper_requests_per_turn),
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: PYTHON_HARNESS_SDK_RUN_NAME_V1.into(),
        wait_name: None,
        marker_executor: ToolExecutorId::new(PYTHON_HARNESS_SDK_DRIVER_MARKER_EXECUTOR_ID_V1)
            .map_err(|error| error.to_string())?,
    })
}

/// Recompiles and validates the exact descriptor of one attachment.
struct Authority {
    access: PythonHarnessSdkAccessV1,
    presentation_kind: CodingPresentation,
    evidence: ProductExtensionEvidence,
}

impl Authority {
    fn compile(
        &self,
        job_config: JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<PythonHarnessSdkDescriptorV1, String> {
        PythonHarnessSdkV1::compile(
            job_config,
            registry,
            target,
            self.presentation_kind,
            self.access,
        )
        .map_err(|error| error.to_string())
    }
}

impl HarnessSdkDescriptorAuthority for Authority {
    fn rebuild_descriptor(
        &self,
        job_config: JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        neutral_descriptor(&self.compile(job_config, registry, target)?)
    }

    fn validate_configuration(
        &self,
        job_config: JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        if !projection.is_for_registry(&registry)
            || !descriptor.outer_projection.is_for_registry(&registry)
            || !descriptor.nested_projection.is_for_registry(&registry)
        {
            return Err("Python SDK projection belongs to a foreign registry".into());
        }
        let target = crate::construction::provider_model_projection_identity(configuration)
            .map_err(|error| error.to_string())?;
        if !matches!(
            target.integration_class(),
            IntegrationClass::Standard | IntegrationClass::Custom
        ) || descriptor.outer_projection.target() != &target
            || descriptor.nested_projection.target() != &target
            || projection.target() != &target
            || self.evidence.configuration_identities()
                != (
                    configuration.selected_identity(),
                    configuration.effective_identity(),
                )
            || self.evidence.target() != target.integration_class()
        {
            return Err(
                "Python SDK descriptor or evidence is foreign to the exact provider configuration"
                    .into(),
            );
        }
        let configured = &configuration.effective().behavior().tools;
        if !matches!(configured.as_slice(), [tool]
            if tool.execution() == ToolExecutionDisposition::Local
                && tool.definition().name().as_str() == PYTHON_HARNESS_SDK_RUN_NAME_V1
                && registry.definition(tool.definition().name()) == Some(tool.definition()))
        {
            return Err("provider configuration does not own exactly the run_py marker".into());
        }
        let exact = self.compile(job_config, registry, target)?;
        let (_, _, catalog, presentation, limits, _) = self.evidence.extension_facts();
        if exact.identity() != descriptor.identity
            || exact.outer_projection().digest() != projection.digest()
            || catalog != exact.catalog_identity()
            || presentation != exact.outer_projection().digest()
            || limits != extension_limits()
        {
            return Err("Python SDK descriptor, receipt, or outer projection drifted".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use plakous_agent::internal::facade_support::StoredCompositionArtifact;

    use super::super::{
        LocalCompositionEngineArtifact, LocalCompositionEngineArtifactKind,
        attachment_binding_identity, restore_stored_artifact,
    };

    /// Both Python profiles record the same worker facts; a recorded SDK
    /// attachment restores with the SDK engine contract, not py-ptc's.
    #[test]
    fn a_recorded_sdk_worker_restores_with_the_sdk_engine_contract()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("monty");
        std::fs::write(&path, b"stand-in worker bytes")?;
        let path = path.canonicalize()?;
        let artifact = LocalCompositionEngineArtifact::monty_pool_harness_sdk_v1(&path)?;
        let ptc = LocalCompositionEngineArtifact::monty_pool_v1(&path)?;
        assert_eq!(artifact.identity(), ptc.identity());
        assert_ne!(
            artifact.engine_contract_identity(),
            ptc.engine_contract_identity()
        );
        let LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
            environment_identity,
            ..
        } = &artifact.kind
        else {
            return Err("the SDK artifact is a Monty worker".into());
        };
        let contract = artifact.engine_contract_identity();
        let stored = StoredCompositionArtifact {
            profile_identity: contract.identity_bytes(),
            artifact_identity: artifact.identity().identity_bytes(),
            binding_identity: attachment_binding_identity(
                contract,
                artifact.identity(),
                contract,
                None,
            )?
            .identity_bytes(),
            engine_contract_identity: contract.identity_bytes(),
            host: environment_identity.clone(),
        };
        let restored = restore_stored_artifact(&stored)?;
        assert_eq!(restored.engine_contract_identity(), contract);
        assert_eq!(restored.identity(), artifact.identity());
        Ok(())
    }
}
