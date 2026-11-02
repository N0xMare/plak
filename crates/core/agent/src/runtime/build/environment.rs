use super::{
    AgentError, AgentErrorKind, CompiledProviderConfiguration, Digest, DigestDomain,
    FrozenToolRegistry, HarnessProfileSpec, LanguageModel, LocalCompositionConstruction,
    ReconstructiveComponent, ResolvedContextSourceFact, ResolvedHarnessPlan, RuntimeComponentKind,
    ToolRuntime, WorkspacePolicy, profile_construction_error, runtime_component,
};
use plakous_tools::ExecutionBackendDescriptor;

#[cfg(test)]
mod tests;

const JOB_CONFIG_ENVIRONMENT_TAG: &[u8] = b"\0plakous-agent/job-limits-v1\0";

pub(super) fn stable_workspace_environment_digest(
    workspace: &WorkspacePolicy,
    backend: &ExecutionBackendDescriptor,
) -> Result<Digest, AgentError> {
    let bytes = stable_workspace_environment_bytes(workspace, backend)?;
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[b"plakous-agent/workspace-facts-v1", &bytes],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "workspace environment facts could not be digested",
        )
    })
}

/// Compiles an explicit profile from the exact tool registry and workspace
/// policy that an [`AgentBuilder`](crate::AgentBuilder) will consume.
///
/// This is the safe consumer seam for explicit product profiles: callers do
/// not reconstruct private tool or execution-environment identity encodings.
/// The resulting plan can be supplied directly to
/// [`AgentBuilder::harness_plan`](crate::AgentBuilder::harness_plan).
///
/// # Errors
///
/// Returns a typed construction error when the registry or workspace cannot
/// be identified, or when the profile does not compile for the selected model.
pub fn compile_explicit_runtime_profile(
    spec: &HarnessProfileSpec,
    model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    tools: &FrozenToolRegistry,
    workspace: &WorkspacePolicy,
    context_sources: Vec<ResolvedContextSourceFact>,
) -> Result<ResolvedHarnessPlan, AgentError> {
    let registry_digest = tools.registry_identity_digest().map_err(|error| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("frozen tool registry could not be digested: {error}"),
        )
    })?;
    let canonical_tool_names = tools
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str().to_owned())
        .collect();
    // An explicit profile is compiled before any environment exists; the
    // builder's backend is the host backend (spec 01 §12).
    let environment_digest = stable_workspace_environment_digest(
        workspace,
        &ExecutionBackendDescriptor::current_host(),
    )?;
    crate::compile_explicit_profile(
        spec,
        model,
        provider_configuration,
        registry_digest,
        canonical_tool_names,
        environment_digest,
        context_sources,
    )
    .map_err(|error| profile_construction_error(&error))
}

pub(in crate::runtime) fn stable_runtime_environment_component(
    workspace: &WorkspacePolicy,
    backend: &ExecutionBackendDescriptor,
    local_composition: Option<&LocalCompositionConstruction>,
    provider_configuration: &CompiledProviderConfiguration,
    job_config: plakous_tools::JobConfig,
) -> Result<ReconstructiveComponent, AgentError> {
    let mut bytes = stable_workspace_environment_bytes(workspace, backend)?;
    let Some(local_composition) = local_composition else {
        append_job_config(&mut bytes, job_config);
        return runtime_component(RuntimeComponentKind::ExecutionEnvironment, bytes);
    };
    bytes.extend_from_slice(b"\0plakous-agent/local-composition-attachment-v2\0");
    for identity in [
        local_composition.attachment.profile,
        local_composition.attachment.artifact,
        local_composition.attachment.binding,
        local_composition.attachment.engine_contract,
        local_composition.descriptor.identity,
        local_composition.descriptor.outer_projection.digest(),
        local_composition.descriptor.nested_projection.digest(),
        provider_configuration.selected_identity().digest(),
        provider_configuration.effective_identity().digest(),
    ] {
        bytes.extend_from_slice(&identity.identity_bytes());
    }
    bytes.extend_from_slice(
        &local_composition
            .integration
            .max_yield_time_ms()
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u64::try_from(local_composition.integration.max_output_tokens())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "local composition output limit exceeds the runtime bound",
                )
            })?
            .to_be_bytes(),
    );
    let host = local_composition.integration.environment_identity();
    bytes.extend_from_slice(
        &u64::try_from(host.len())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "local composition host identity exceeds the runtime bound",
                )
            })?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&host);
    append_job_config(&mut bytes, job_config);
    runtime_component(RuntimeComponentKind::ExecutionEnvironment, bytes)
}

pub(crate) fn stored_composition_artifact(
    mut bytes: &[u8],
) -> Result<Option<crate::facade_support::StoredCompositionArtifact>, AgentError> {
    fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
        let value = bytes.get(..len)?;
        *bytes = &bytes[len..];
        Some(value)
    }
    fn framed<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
        let len = u64::from_be_bytes(take(bytes, 8)?.try_into().ok()?);
        take(bytes, usize::try_from(len).ok()?)
    }
    fn digest(bytes: &mut &[u8]) -> Option<Vec<u8>> {
        let original = *bytes;
        framed(bytes)?;
        framed(bytes)?;
        take(bytes, 36)?;
        Some(original[..original.len() - bytes.len()].to_vec())
    }
    let invalid = || {
        AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "stored composition environment is invalid",
        )
    };
    let prefix = b"plakous-agent/workspace-environment-v1\0";
    if take(&mut bytes, prefix.len()) != Some(prefix.as_slice()) {
        return Err(invalid());
    }
    let os_end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(invalid)?;
    take(&mut bytes, os_end + 1).ok_or_else(invalid)?;
    let policy = b"host-path-policy-only\0";
    if take(&mut bytes, policy.len()) != Some(policy.as_slice()) {
        return Err(invalid());
    }
    framed(&mut bytes).ok_or_else(invalid)?;
    framed(&mut bytes).ok_or_else(invalid)?;
    if bytes.starts_with(JOB_CONFIG_ENVIRONMENT_TAG) {
        if bytes.len() != JOB_CONFIG_ENVIRONMENT_TAG.len() + 16 {
            return Err(invalid());
        }
        stored_job_config(bytes)?;
        return Ok(None);
    }
    let tag = b"\0plakous-agent/local-composition-attachment-v2\0";
    if take(&mut bytes, tag.len()) != Some(tag.as_slice()) {
        return Err(invalid());
    }
    let profile_identity = digest(&mut bytes).ok_or_else(invalid)?;
    let artifact_identity = digest(&mut bytes).ok_or_else(invalid)?;
    let binding_identity = digest(&mut bytes).ok_or_else(invalid)?;
    let engine_contract_identity = digest(&mut bytes).ok_or_else(invalid)?;
    for _ in 0..5 {
        digest(&mut bytes).ok_or_else(invalid)?;
    }
    take(&mut bytes, 16).ok_or_else(invalid)?; // presentation limits
    let host = framed(&mut bytes).ok_or_else(invalid)?.to_vec();
    if bytes.len() != JOB_CONFIG_ENVIRONMENT_TAG.len() + 16 {
        return Err(invalid());
    }
    stored_job_config(bytes)?;
    Ok(Some(crate::facade_support::StoredCompositionArtifact {
        profile_identity,
        artifact_identity,
        binding_identity,
        engine_contract_identity,
        host,
    }))
}

fn append_job_config(bytes: &mut Vec<u8>, config: plakous_tools::JobConfig) {
    bytes.extend_from_slice(JOB_CONFIG_ENVIRONMENT_TAG);
    bytes.extend_from_slice(&config.foreground_seconds().to_be_bytes());
    bytes.extend_from_slice(&config.max_runtime_hours().to_be_bytes());
}

pub(crate) fn stored_job_config(bytes: &[u8]) -> Result<plakous_tools::JobConfig, AgentError> {
    let invalid = || {
        AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "stored job environment is invalid",
        )
    };
    let start = bytes
        .len()
        .checked_sub(JOB_CONFIG_ENVIRONMENT_TAG.len() + 16)
        .ok_or_else(invalid)?;
    if &bytes[start..start + JOB_CONFIG_ENVIRONMENT_TAG.len()] != JOB_CONFIG_ENVIRONMENT_TAG {
        return Err(invalid());
    }
    let values = &bytes[start + JOB_CONFIG_ENVIRONMENT_TAG.len()..];
    let budget = u64::from_be_bytes(values[..8].try_into().map_err(|_| invalid())?);
    let hours = u64::from_be_bytes(values[8..].try_into().map_err(|_| invalid())?);
    plakous_tools::JobConfig::new(budget, hours).map_err(|_| invalid())
}

pub(in crate::runtime) fn live_job_config(
    tools: &ToolRuntime,
) -> Result<plakous_tools::JobConfig, AgentError> {
    let limits = tools.environment().config().limits();
    plakous_tools::JobConfig::new(
        limits.foreground_seconds(),
        limits.session_duration().as_secs() / 3600,
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "live job environment is invalid",
        )
    })
}

/// The run-stable workspace facts: the workspace OS the backend descriptor
/// names, then the canonical root and cwd.
pub(in crate::runtime) fn stable_workspace_environment_bytes(
    workspace: &WorkspacePolicy,
    backend: &ExecutionBackendDescriptor,
) -> Result<Vec<u8>, AgentError> {
    let root = workspace.root().as_os_str().as_encoded_bytes();
    let cwd = workspace.cwd().as_os_str().as_encoded_bytes();
    let root_len = u64::try_from(root.len()).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "workspace root identity exceeds the runtime bound",
        )
    })?;
    let cwd_len = u64::try_from(cwd.len()).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "workspace current-directory identity exceeds the runtime bound",
        )
    })?;
    let mut bytes = Vec::with_capacity(
        64_usize
            .saturating_add(root.len())
            .saturating_add(cwd.len()),
    );
    bytes.extend_from_slice(b"plakous-agent/workspace-environment-v1\0");
    bytes.extend_from_slice(backend.operating_system().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(b"host-path-policy-only\0");
    bytes.extend_from_slice(&root_len.to_be_bytes());
    bytes.extend_from_slice(root);
    bytes.extend_from_slice(&cwd_len.to_be_bytes());
    bytes.extend_from_slice(cwd);
    Ok(bytes)
}
