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
    let backend = ExecutionBackendDescriptor::host_for_platform("linux", HostPathFormat::UnixUtf8);
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
        "4ae9c45752c449850a84b55c7ac2a789f9431f54a2e018324ac687d8ce21e729"
    );
    Ok(())
}

#[test]
fn concrete_host_authority_is_excluded_from_stable_descriptor_digest()
-> Result<(), Box<dyn std::error::Error>> {
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000042")?;
    let first = ExecutionBackendIdentity {
        descriptor: ExecutionBackendDescriptor::host_for_platform(
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
