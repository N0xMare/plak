use super::{
    AgentError, AgentErrorKind, Digest, DigestDomain, ExecutionBackendDescriptor, WorkspacePolicy,
    append_job_config, stable_workspace_environment_bytes, stored_composition_artifact,
    stored_job_config,
};

#[test]
fn workspace_facts_name_the_backend_os_and_keep_the_host_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = WorkspacePolicy::host(directory.path(), ".")?;
    let host = ExecutionBackendDescriptor::current_host();
    let root = workspace.root().as_os_str().as_encoded_bytes();
    let cwd = workspace.cwd().as_os_str().as_encoded_bytes();
    let mut expected = b"plakous-agent/workspace-environment-v1\0".to_vec();
    expected.extend_from_slice(std::env::consts::OS.as_bytes());
    expected.extend_from_slice(b"\0host-path-policy-only\0");
    expected.extend_from_slice(&u64::try_from(root.len())?.to_be_bytes());
    expected.extend_from_slice(root);
    expected.extend_from_slice(&u64::try_from(cwd.len())?.to_be_bytes());
    expected.extend_from_slice(cwd);
    assert_eq!(
        stable_workspace_environment_bytes(&workspace, &host)?,
        expected
    );

    let mut other: serde_json::Value = serde_json::to_value(&host)?;
    other["operating_system"] = "freebsd".into();
    let other: ExecutionBackendDescriptor = serde_json::from_value(other)?;
    let bytes = stable_workspace_environment_bytes(&workspace, &other)?;
    let prefix = b"plakous-agent/workspace-environment-v1\0".len();
    assert!(bytes[prefix..].starts_with(b"freebsd\0host-path-policy-only\0"));
    Ok(())
}

#[test]
fn composition_environment_reader_is_length_framed_and_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = WorkspacePolicy::host(directory.path(), ".")?;
    let mut bytes = stable_workspace_environment_bytes(
        &workspace,
        &ExecutionBackendDescriptor::current_host(),
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
