use super::{
    AgentError, AgentErrorKind, Digest, DigestDomain, ExecutionBackendDescriptor, WorkspacePolicy,
    append_job_config, require_same_runner, stable_workspace_environment_bytes,
    stored_composition_artifact, stored_job_config,
};

#[test]
fn workspace_facts_name_the_backend_os_and_runner() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = WorkspacePolicy::host(directory.path(), ".")?;
    let host = ExecutionBackendDescriptor::local_runner();
    let root = workspace.root().as_os_str().as_encoded_bytes();
    let cwd = workspace.cwd().as_os_str().as_encoded_bytes();
    let mut expected = b"plakous-agent/workspace-environment-v2\0".to_vec();
    expected.extend_from_slice(std::env::consts::OS.as_bytes());
    expected.extend_from_slice(b"\0local\0host-path-policy-only\0");
    expected.extend_from_slice(&u64::try_from(root.len())?.to_be_bytes());
    expected.extend_from_slice(root);
    expected.extend_from_slice(&u64::try_from(cwd.len())?.to_be_bytes());
    expected.extend_from_slice(cwd);
    assert_eq!(
        stable_workspace_environment_bytes(&workspace, &host)?,
        expected
    );

    // A runner descriptor for the other supported OS decodes when its
    // containment is that OS's, whatever this process runs on; a crossed
    // containment claim and an unsupported runner OS are refused.
    let (os, filesystem, crossed) = if std::env::consts::OS == "linux" {
        (
            "macos",
            "HostPathPolicyOnly",
            "LinuxDescriptorRelativeNoFollow",
        )
    } else {
        (
            "linux",
            "LinuxDescriptorRelativeNoFollow",
            "HostPathPolicyOnly",
        )
    };
    let mut other: serde_json::Value = serde_json::to_value(&host)?;
    other["operating_system"] = os.into();
    other["containment"]["filesystem"] = filesystem.into();
    let decoded: ExecutionBackendDescriptor = serde_json::from_value(other.clone())?;
    let bytes = stable_workspace_environment_bytes(&workspace, &decoded)?;
    let prefix = b"plakous-agent/workspace-environment-v2\0".len();
    let mut facts = os.as_bytes().to_vec();
    facts.extend_from_slice(b"\0local\0host-path-policy-only\0");
    assert!(bytes[prefix..].starts_with(&facts));
    other["containment"]["filesystem"] = crossed.into();
    assert!(serde_json::from_value::<ExecutionBackendDescriptor>(other.clone()).is_err());
    other["operating_system"] = "freebsd".into();
    other["containment"]["filesystem"] = "HostPathPolicyOnly".into();
    assert!(serde_json::from_value::<ExecutionBackendDescriptor>(other).is_err());
    Ok(())
}

/// Every rebind of a line keeps its runner: a target on another runner is
/// refused and names both runners.
#[test]
fn a_transition_onto_another_runner_is_refused_by_name() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = WorkspacePolicy::host(directory.path(), ".")?;
    let local = ExecutionBackendDescriptor::local_runner();
    let recorded = stable_workspace_environment_bytes(&workspace, &local)?;
    require_same_runner("rebind", &recorded, &local)?;

    let mut remote: serde_json::Value = serde_json::to_value(&local)?;
    remote["runner_id"] = "build-box".into();
    let remote: ExecutionBackendDescriptor = serde_json::from_value(remote)?;
    let refused = require_same_runner("rebind", &recorded, &remote)
        .err()
        .ok_or("a rebind onto another runner must be refused")?;
    assert_eq!(refused.kind(), AgentErrorKind::InvalidInput);
    assert_eq!(
        refused.binding_change(),
        Some(&crate::BindingChange::Runner {
            recorded: "local".to_owned(),
            current: "build-box".to_owned(),
        })
    );
    assert_eq!(
        refused.binding_change().map(ToString::to_string).as_deref(),
        Some("runner (recorded `local`, this build `build-box`)")
    );
    Ok(())
}

#[test]
fn composition_environment_reader_is_length_framed_and_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = WorkspacePolicy::host(directory.path(), ".")?;
    let mut bytes = stable_workspace_environment_bytes(
        &workspace,
        &ExecutionBackendDescriptor::local_runner(),
    )?;
    let workspace_len = bytes.len();
    append_job_config(&mut bytes, plakous_tools::JobConfig::default());
    assert!(stored_composition_artifact(&bytes)?.is_none());
    bytes.truncate(workspace_len);
    bytes.extend_from_slice(b"\0plakous-agent/local-composition-attachment-v2\0");
    let identity =
        Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"fixture"])?.identity_bytes();
    for _ in 0..9 {
        bytes.extend_from_slice(&identity);
    }
    bytes.extend_from_slice(&10_000_u64.to_be_bytes());
    bytes.extend_from_slice(&10_000_u64.to_be_bytes());
    let host_length_offset = bytes.len();
    let host = b"rquickjs-host/fixture\0plakous-agent/job-limits-v1\0";
    bytes.extend_from_slice(&u64::try_from(host.len())?.to_be_bytes());
    bytes.extend_from_slice(host);
    append_job_config(&mut bytes, plakous_tools::JobConfig::default());
    let saved = stored_composition_artifact(&bytes)?.ok_or("missing artifact")?;
    assert_eq!(saved.host, host);
    assert_eq!(saved.artifact_identity, identity);
    assert_eq!(saved.engine_contract_identity, identity);
    for end in 0..bytes.len() {
        assert!(stored_composition_artifact(&bytes[..end]).is_err());
    }
    bytes[host_length_offset..host_length_offset + 8].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(stored_composition_artifact(&bytes).is_err());
    Ok(())
}

#[test]
fn stored_job_config_refuses_missing_or_invalid_bounds() -> Result<(), AgentError> {
    let mut valid = b"environment identity".to_vec();
    append_job_config(&mut valid, plakous_tools::JobConfig::default());
    assert_eq!(
        stored_job_config(&valid)?,
        plakous_tools::JobConfig::default(),
    );
    let mut out_of_range = valid.clone();
    let budget_start = out_of_range.len() - 16;
    out_of_range[budget_start..budget_start + 8].copy_from_slice(&0_u64.to_be_bytes());
    let mut wrong_tag = valid.clone();
    wrong_tag[budget_start - 1] ^= 1;
    for invalid in [
        Vec::new(),
        b"environment identity".to_vec(),
        valid[..valid.len() - 1].to_vec(),
        out_of_range,
        wrong_tag,
    ] {
        assert_eq!(
            stored_job_config(&invalid).err().map(|error| error.kind()),
            Some(AgentErrorKind::IncompatibleSession),
        );
    }
    Ok(())
}
