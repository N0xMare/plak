use std::str::FromStr;

use super::*;

fn source_authority(id: &str) -> Result<DelegationSourceAuthorityId, ExecutionEnvironmentError> {
    DelegationSourceAuthorityId::new(id)
}

#[test]
fn digest_has_a_fixed_cross_implementation_vector() -> Result<(), Box<dyn std::error::Error>> {
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000041")?;
    let config = ExecutionEnvironmentConfig::new(
        ExecutionEnvironmentLimits::default(),
        [
            EnvironmentDelegation::new(
                EnvironmentVariableName::new("LANG")?,
                source_authority("source.environment.fixture")?,
                DelegationSourceRevision::new(3)?,
            ),
            EnvironmentDelegation::new(
                EnvironmentVariableName::new("PATH")?,
                source_authority("source.environment.fixture")?,
                DelegationSourceRevision::new(3)?,
            ),
        ],
        [
            CredentialDelegation::new(
                CredentialSlotId::new("credential.primary/default")?,
                source_authority("source.credential.fixture")?,
                DelegationSourceRevision::new(5)?,
                DelegatedAuthorityId::new("authority.account-a.access-standard.billing-included")?,
            ),
            CredentialDelegation::new(
                CredentialSlotId::new("credential.secondary/default")?,
                source_authority("source.credential.fixture")?,
                DelegationSourceRevision::new(5)?,
                DelegatedAuthorityId::new("authority.account-b.access-standard.billing-metered")?,
            ),
        ],
    )?;
    let backend =
        ExecutionBackendDescriptor::local_runner_for_platform("linux", HostPathFormat::UnixUtf8);
    let digest = environment_digest(
        &backend,
        ExecutionEnvironmentScope::Run(run_id),
        b"/canonical/workspace",
        b"/canonical/workspace/src",
        HostFilesystemObjectIdentity {
            device: 7,
            inode: 41,
        },
        &config,
    )?;

    assert_eq!(digest.domain(), DigestDomain::ExecutionEnvironment);
    assert_eq!(digest.version(), EXECUTION_ENVIRONMENT_DIGEST_VERSION);
    assert_eq!(
        digest.to_string(),
        "61caf8ca452f50874b228088d610b2e59f6a76a4307d3955353f5ad2aed370e0"
    );
    Ok(())
}

#[test]
fn concrete_host_authority_is_excluded_from_stable_descriptor_digest()
-> Result<(), Box<dyn std::error::Error>> {
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000042")?;
    let first = ExecutionBackendIdentity {
        descriptor: ExecutionBackendDescriptor::local_runner_for_platform(
            "linux",
            HostPathFormat::UnixUtf8,
        ),
        authority_id: ExecutionBackendAuthorityId::new([0x41; 16])?,
        authority_revision: ExecutionBackendAuthorityRevision::host(),
    };
    let second = ExecutionBackendIdentity {
        descriptor: first.descriptor.clone(),
        authority_id: ExecutionBackendAuthorityId::new([0x42; 16])?,
        authority_revision: ExecutionBackendAuthorityRevision::host(),
    };
    let config = ExecutionEnvironmentConfig::default();
    let first_digest = environment_digest(
        first.descriptor(),
        ExecutionEnvironmentScope::Run(run_id),
        b"/canonical/workspace",
        b"/canonical/workspace/src",
        HostFilesystemObjectIdentity {
            device: 7,
            inode: 41,
        },
        &config,
    )?;
    let second_digest = environment_digest(
        second.descriptor(),
        ExecutionEnvironmentScope::Run(run_id),
        b"/canonical/workspace",
        b"/canonical/workspace/src",
        HostFilesystemObjectIdentity {
            device: 7,
            inode: 41,
        },
        &config,
    )?;

    assert_ne!(first, second);
    assert_eq!(first_digest, second_digest);
    Ok(())
}

#[test]
fn generation_stamp_is_excluded_from_environment_digest() -> Result<(), Box<dyn std::error::Error>>
{
    use std::sync::atomic::AtomicU64;

    let workspace = tempfile::tempdir()?;
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000042")?;
    let stamped = WorkspacePolicy::host(workspace.path(), ".")?
        .with_generation_stamp(Arc::new(AtomicU64::new(99)));
    let with_stamp = ExecutionEnvironment::host_with_policy(
        ExecutionEnvironmentScope::Run(run_id),
        stamped,
        ExecutionEnvironmentConfig::default(),
    )?;
    let plain = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    assert_eq!(
        with_stamp.descriptor().digest(),
        plain.descriptor().digest()
    );
    assert_eq!(with_stamp.workspace().generation_stamp(), Some(99));
    assert_eq!(plain.workspace().generation_stamp(), None);
    Ok(())
}

/// The in-process host backend is the local runner, and the descriptor names
/// it on the wire and in the digest.
#[test]
fn the_host_backend_is_the_local_runner() -> Result<(), Box<dyn std::error::Error>> {
    let local = ExecutionBackendDescriptor::local_runner();
    assert_eq!(local.kind(), ExecutionBackendKind::Runner);
    assert_eq!(local.runner_id(), Some(&RunnerId::local()));
    assert!(RunnerId::local().is_local());
    let wire = serde_json::to_value(&local)?;
    assert_eq!(wire["kind"], "Runner");
    assert_eq!(wire["runner_id"], "local");
    let restored: ExecutionBackendDescriptor = serde_json::from_value(wire)?;
    assert_eq!(restored, local);

    let fake = ExecutionBackendDescriptor::deterministic_fake();
    assert_eq!(fake.runner_id(), None);
    assert!(serde_json::to_value(&fake)?.get("runner_id").is_none());
    Ok(())
}

/// A runner backend must name its runner and the fake must not; runner ids
/// are lowercase identifiers.
#[test]
fn a_descriptor_names_a_runner_exactly_when_it_is_one() -> Result<(), Box<dyn std::error::Error>> {
    let mut unnamed = serde_json::to_value(ExecutionBackendDescriptor::local_runner())?;
    unnamed.as_object_mut().ok_or("object")?.remove("runner_id");
    assert!(serde_json::from_value::<ExecutionBackendDescriptor>(unnamed).is_err());
    let mut named_fake = serde_json::to_value(ExecutionBackendDescriptor::deterministic_fake())?;
    named_fake["runner_id"] = "local".into();
    assert!(serde_json::from_value::<ExecutionBackendDescriptor>(named_fake).is_err());
    let mut uppercase = serde_json::to_value(ExecutionBackendDescriptor::local_runner())?;
    uppercase["runner_id"] = "Tars".into();
    assert!(serde_json::from_value::<ExecutionBackendDescriptor>(uppercase).is_err());
    assert_eq!(RunnerId::new("build-box")?.as_str(), "build-box");
    assert!(RunnerId::new("").is_err());
    assert!(RunnerId::new("x".repeat(129)).is_err());
    Ok(())
}

/// The runner is part of the environment's identity: the same workspace on
/// another runner digests differently.
#[test]
fn the_runner_is_inside_the_environment_digest() -> Result<(), Box<dyn std::error::Error>> {
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000043")?;
    let config = ExecutionEnvironmentConfig::default();
    let local =
        ExecutionBackendDescriptor::local_runner_for_platform("linux", HostPathFormat::UnixUtf8);
    let mut remote = local.clone();
    remote.runner_id = Some(RunnerId::new("build-box")?);
    let digest = |backend: &ExecutionBackendDescriptor| {
        environment_digest(
            backend,
            ExecutionEnvironmentScope::Run(run_id),
            b"/canonical/workspace",
            b"/canonical/workspace",
            HostFilesystemObjectIdentity {
                device: 7,
                inode: 41,
            },
            &config,
        )
    };
    assert_ne!(digest(&local)?, digest(&remote)?);
    Ok(())
}
