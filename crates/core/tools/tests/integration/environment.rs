//! Public conformance tests for execution-environment identity and authority.

use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use plakous_model::{AttemptId, DigestAlgorithm, DigestDomain, RunId};
use plakous_tools::{
    CredentialDelegation, CredentialSlotId, DelegatedAuthorityId, DelegationSourceAuthorityId,
    DelegationSourceRevision, EnvironmentDelegation, EnvironmentVariableName, ExecutionBackendKind,
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentDescriptor,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
    ExecutionEnvironmentScope, FilesystemContainment, HostPathFormat,
    MAX_ENVIRONMENT_ALLOWLIST_ENTRIES, MAX_ENVIRONMENT_CLEANUP_DURATION,
    MAX_ENVIRONMENT_EXECUTION_DURATION, MAX_ENVIRONMENT_NAME_BYTES, MAX_ENVIRONMENT_OUTPUT_BYTES,
    NetworkContainment, ProcessContainment,
};

type TestError = Box<dyn std::error::Error>;

fn run_id(value: u8) -> Result<RunId, TestError> {
    Ok(RunId::from_str(&format!(
        "00000000-0000-0000-0000-{value:012x}"
    ))?)
}

fn attempt_id(value: u8) -> Result<AttemptId, TestError> {
    Ok(AttemptId::from_str(&format!(
        "00000000-0000-0000-0000-{value:012x}"
    ))?)
}

fn source_id(value: &str) -> Result<DelegationSourceAuthorityId, TestError> {
    Ok(DelegationSourceAuthorityId::new(value)?)
}

fn source_revision(value: u64) -> Result<DelegationSourceRevision, TestError> {
    Ok(DelegationSourceRevision::new(value)?)
}

fn environment_delegation(
    name: &str,
    source: &str,
    revision: u64,
) -> Result<EnvironmentDelegation, TestError> {
    Ok(EnvironmentDelegation::new(
        EnvironmentVariableName::new(name)?,
        source_id(source)?,
        source_revision(revision)?,
    ))
}

fn host_environment_delegation(name: &str) -> Result<EnvironmentDelegation, TestError> {
    Ok(EnvironmentDelegation::host_snapshot(
        EnvironmentVariableName::new(name)?,
    )?)
}

fn credential_delegation(
    slot: &str,
    source: &str,
    revision: u64,
    authority: &str,
) -> Result<CredentialDelegation, TestError> {
    Ok(CredentialDelegation::new(
        CredentialSlotId::new(slot)?,
        source_id(source)?,
        source_revision(revision)?,
        DelegatedAuthorityId::new(authority)?,
    ))
}

fn delegations() -> Result<(Vec<EnvironmentDelegation>, Vec<CredentialDelegation>), TestError> {
    Ok((
        vec![
            host_environment_delegation("LANG")?,
            host_environment_delegation("PATH")?,
        ],
        vec![
            credential_delegation(
                "credential.primary/default",
                "source.credential.default",
                5,
                "authority.account-a.access-standard.billing-included",
            )?,
            credential_delegation(
                "credential.secondary/default",
                "source.credential.default",
                5,
                "authority.account-b.access-standard.billing-metered",
            )?,
        ],
    ))
}

fn config_with_limits(
    limits: ExecutionEnvironmentLimits,
) -> Result<ExecutionEnvironmentConfig, TestError> {
    let (environment, credentials) = delegations()?;
    Ok(ExecutionEnvironmentConfig::new(
        limits,
        environment,
        credentials,
    )?)
}

fn config() -> Result<ExecutionEnvironmentConfig, TestError> {
    config_with_limits(ExecutionEnvironmentLimits::default())
}

fn host(
    root: &Path,
    cwd: &str,
    scope: ExecutionEnvironmentScope,
    config: ExecutionEnvironmentConfig,
) -> Result<ExecutionEnvironment, ExecutionEnvironmentError> {
    ExecutionEnvironment::host(scope, root, cwd, config)
}

#[test]
fn host_authority_allocations_are_distinct_and_paths_are_canonical() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("src"))?;
    let scope = ExecutionEnvironmentScope::Run(run_id(1)?);
    let first = host(workspace.path(), "src", scope, config()?)?;
    let second = host(workspace.path(), "src/.", scope, config()?)?;

    assert_eq!(first.descriptor(), second.descriptor());
    assert_ne!(
        first.backend().authority_id(),
        second.backend().authority_id()
    );
    assert_eq!(first.root(), second.root());
    assert_eq!(first.cwd(), second.cwd());
    assert_eq!(first.config(), second.config());
    assert_eq!(first.root(), workspace.path().canonicalize()?);
    assert_eq!(first.cwd(), workspace.path().join("src").canonicalize()?);
    assert_eq!(first.relative_cwd(), Path::new("src"));
    assert_eq!(first.backend().kind(), ExecutionBackendKind::Host);
    assert_eq!(first.backend().semantic_revision().get(), 15);
    assert_eq!(first.backend().authority_revision().get(), 1);
    assert_eq!(first.backend().operating_system(), std::env::consts::OS);
    assert_eq!(first.backend().path_format(), HostPathFormat::UnixUtf8);
    #[cfg(windows)]
    assert_eq!(first.backend().path_format(), HostPathFormat::WindowsUtf8);
    let containment = first.backend().containment();
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            containment.filesystem(),
            FilesystemContainment::LinuxDescriptorRelativeNoFollow
        );
        assert_eq!(
            containment.process(),
            ProcessContainment::InheritsHostAuthority
        );
        assert_eq!(
            containment.network(),
            NetworkContainment::InheritsHostAuthority
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(
            containment.filesystem(),
            FilesystemContainment::HostPathPolicyOnly
        );
        assert_eq!(
            containment.process(),
            ProcessContainment::InheritsHostAuthority
        );
        assert_eq!(
            containment.network(),
            NetworkContainment::InheritsHostAuthority
        );
    }
    assert_eq!(
        first.descriptor().digest().algorithm(),
        DigestAlgorithm::Sha256
    );
    assert_eq!(
        first.descriptor().digest().domain(),
        DigestDomain::ExecutionEnvironment
    );
    assert_eq!(
        first.descriptor().digest().version(),
        plakous_tools::EXECUTION_ENVIRONMENT_DIGEST_VERSION
    );
    assert!(first.matches_descriptor(first.descriptor()));
    first.verify_descriptor(first.descriptor())?;
    Ok(())
}

#[test]
fn symlinked_workspace_alias_has_one_canonical_path_under_distinct_authority()
-> Result<(), TestError> {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir()?;
    let workspace = parent.path().join("workspace");
    fs::create_dir(&workspace)?;
    let alias = parent.path().join("workspace-alias");
    symlink(&workspace, &alias)?;
    let scope = ExecutionEnvironmentScope::Run(run_id(2)?);

    let direct = host(&workspace, ".", scope, config()?)?;
    let through_alias = host(&alias, ".", scope, config()?)?;
    assert_eq!(direct.descriptor(), through_alias.descriptor());
    assert_ne!(
        direct.backend().authority_id(),
        through_alias.backend().authority_id()
    );
    assert_eq!(direct.root(), through_alias.root());
    assert_eq!(direct.cwd(), through_alias.cwd());
    Ok(())
}

#[test]
fn delegation_order_is_canonical_and_lookup_is_exact() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let (environment, credentials) = delegations()?;
    let first = ExecutionEnvironmentConfig::new(
        ExecutionEnvironmentLimits::default(),
        environment.clone(),
        credentials.clone(),
    )?;
    let second = ExecutionEnvironmentConfig::new(
        ExecutionEnvironmentLimits::default(),
        environment.into_iter().rev(),
        credentials.into_iter().rev(),
    )?;
    let scope = ExecutionEnvironmentScope::Run(run_id(3)?);
    let first_environment = host(workspace.path(), ".", scope, first)?;
    let second_environment = host(workspace.path(), ".", scope, second)?;

    assert_eq!(
        first_environment.descriptor(),
        second_environment.descriptor()
    );
    assert_eq!(first_environment.config(), second_environment.config());
    let path = EnvironmentVariableName::new("PATH")?;
    let slot = CredentialSlotId::new("credential.primary/default")?;
    assert_eq!(
        first_environment
            .config()
            .environment_delegations()
            .iter()
            .find(|binding| binding.name() == &path)
            .ok_or("PATH delegation missing")?
            .source_revision()
            .get(),
        1
    );
    assert!(
        first_environment
            .config()
            .credential_delegations()
            .iter()
            .any(|binding| binding.slot() == &slot)
    );
    Ok(())
}

#[test]
fn defaults_delegate_nothing_and_limits_are_bounded() {
    let config = ExecutionEnvironmentConfig::default();
    assert_eq!(config.environment_delegations(), []);
    assert_eq!(config.credential_delegations(), []);
    let limits = config.limits();
    assert!((1..=MAX_ENVIRONMENT_OUTPUT_BYTES).contains(&limits.max_stdout_bytes()));
    assert!((1..=MAX_ENVIRONMENT_OUTPUT_BYTES).contains(&limits.max_stderr_bytes()));
    assert!((1..=MAX_ENVIRONMENT_OUTPUT_BYTES).contains(&limits.max_artifact_bytes()));
    assert!(limits.execution_duration() <= MAX_ENVIRONMENT_EXECUTION_DURATION);
    assert!(limits.cleanup_duration() <= MAX_ENVIRONMENT_CLEANUP_DURATION);
}

#[test]
fn every_delegated_authority_component_invalidates_identity() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let scope = ExecutionEnvironmentScope::Run(run_id(4)?);
    let baseline = host(workspace.path(), ".", scope, config()?)?;
    let baseline_digest = baseline.descriptor().digest();

    let environment_mutations = [
        environment_delegation("LANG", "source.environment.changed", 3)?,
        environment_delegation("LANG", "source.environment.default", 4)?,
    ];
    for changed in environment_mutations {
        let (_, credentials) = delegations()?;
        let candidate = ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::default(),
            [changed, host_environment_delegation("PATH")?],
            credentials,
        )?;
        assert_eq!(
            host(workspace.path(), ".", scope, candidate)
                .err()
                .map(|error| error.kind()),
            Some(ExecutionEnvironmentErrorKind::InvalidDelegationSourceAuthority)
        );
    }

    let credential_mutations = [
        credential_delegation(
            "credential.primary/default",
            "source.credential.changed",
            5,
            "authority.account-a.access-standard.billing-included",
        )?,
        credential_delegation(
            "credential.primary/default",
            "source.credential.default",
            6,
            "authority.account-a.access-standard.billing-included",
        )?,
        credential_delegation(
            "credential.primary/default",
            "source.credential.default",
            5,
            "authority.account-c.access-restricted.billing-disabled",
        )?,
    ];
    for changed in credential_mutations {
        let (environment, mut credentials) = delegations()?;
        credentials[0] = changed;
        let candidate = ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::default(),
            environment,
            credentials,
        )?;
        assert_ne!(
            host(workspace.path(), ".", scope, candidate)?
                .descriptor()
                .digest(),
            baseline_digest
        );
    }
    Ok(())
}

#[test]
fn every_limit_field_independently_invalidates_identity() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let scope = ExecutionEnvironmentScope::Run(run_id(5)?);
    let baseline_limits = ExecutionEnvironmentLimits::new(
        100,
        100,
        150,
        1_000,
        Duration::from_secs(30),
        Duration::from_secs(5),
    )?;
    let baseline = host(
        workspace.path(),
        ".",
        scope,
        config_with_limits(baseline_limits)?,
    )?;
    let baseline_digest = baseline.descriptor().digest();
    let mutations = [
        ExecutionEnvironmentLimits::new(
            90,
            100,
            150,
            1_000,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )?,
        ExecutionEnvironmentLimits::new(
            100,
            90,
            150,
            1_000,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )?,
        ExecutionEnvironmentLimits::new(
            100,
            100,
            160,
            1_000,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )?,
        ExecutionEnvironmentLimits::new(
            100,
            100,
            150,
            1_001,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )?,
        ExecutionEnvironmentLimits::new(
            100,
            100,
            150,
            1_000,
            Duration::from_secs(31),
            Duration::from_secs(5),
        )?,
        ExecutionEnvironmentLimits::new(
            100,
            100,
            150,
            1_000,
            Duration::from_secs(30),
            Duration::from_secs(6),
        )?,
    ];

    for limits in mutations {
        let candidate = host(workspace.path(), ".", scope, config_with_limits(limits)?)?;
        assert_ne!(candidate.descriptor().digest(), baseline_digest);
    }
    Ok(())
}

#[test]
fn path_and_scope_changes_invalidate_identity() -> Result<(), TestError> {
    let first_workspace = tempfile::tempdir()?;
    fs::create_dir(first_workspace.path().join("nested"))?;
    let second_workspace = tempfile::tempdir()?;
    let run = run_id(6)?;
    let baseline = host(
        first_workspace.path(),
        ".",
        ExecutionEnvironmentScope::Run(run),
        config()?,
    )?;
    let baseline_digest = baseline.descriptor().digest();

    let candidates = [
        host(
            first_workspace.path(),
            "nested",
            ExecutionEnvironmentScope::Run(run),
            config()?,
        )?,
        host(
            second_workspace.path(),
            ".",
            ExecutionEnvironmentScope::Run(run),
            config()?,
        )?,
        host(
            first_workspace.path(),
            ".",
            ExecutionEnvironmentScope::Attempt {
                run_id: run,
                attempt_id: attempt_id(7)?,
            },
            config()?,
        )?,
    ];
    for candidate in candidates {
        assert_ne!(candidate.descriptor().digest(), baseline_digest);
    }
    Ok(())
}

#[test]
fn descriptor_serde_is_reconstruction_complete_and_live_verification_fails_closed()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = host(
        workspace.path(),
        ".",
        ExecutionEnvironmentScope::Run(run_id(8)?),
        config()?,
    )?;
    let encoded = serde_json::to_value(environment.descriptor())?;
    let round_trip: ExecutionEnvironmentDescriptor = serde_json::from_value(encoded.clone())?;
    assert_eq!(environment.descriptor(), &round_trip);
    assert!(environment.matches_descriptor(&round_trip));
    environment.verify_descriptor(&round_trip)?;

    let mut changed_scope = encoded.clone();
    changed_scope["scope"] = serde_json::json!({ "Run": run_id(9)? });
    assert!(serde_json::from_value::<ExecutionEnvironmentDescriptor>(changed_scope).is_err());
    let other_scope = host(
        workspace.path(),
        ".",
        ExecutionEnvironmentScope::Run(run_id(9)?),
        config()?,
    )?;
    assert!(!environment.matches_descriptor(other_scope.descriptor()));
    assert_eq!(
        environment
            .verify_descriptor(other_scope.descriptor())
            .err()
            .map(|error| error.kind()),
        Some(ExecutionEnvironmentErrorKind::IdentityMismatch)
    );

    let mut changed_backend = encoded.clone();
    changed_backend["backend"]["semantic_revision"] = serde_json::json!(u64::MAX);
    assert!(serde_json::from_value::<ExecutionEnvironmentDescriptor>(changed_backend).is_err());

    let mut changed_digest = encoded.clone();
    let first_byte = changed_digest["digest"]["bytes"][0]
        .as_u64()
        .ok_or("digest byte missing")?;
    changed_digest["digest"]["bytes"][0] = serde_json::json!((first_byte + 1) % 256);
    assert!(serde_json::from_value::<ExecutionEnvironmentDescriptor>(changed_digest).is_err());

    for (field, value) in [
        ("domain", serde_json::json!("Approval")),
        ("version", serde_json::json!(u32::MAX)),
        ("algorithm", serde_json::json!("Unsupported")),
    ] {
        let mut invalid = encoded.clone();
        invalid["digest"][field] = value;
        assert!(serde_json::from_value::<ExecutionEnvironmentDescriptor>(invalid).is_err());
    }
    let mut unknown = encoded;
    unknown["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ExecutionEnvironmentDescriptor>(unknown).is_err());
    Ok(())
}

#[test]
fn constructors_reject_ambiguous_delegations_and_invalid_values() -> Result<(), TestError> {
    for invalid in ["", "Path", "path", "1PATH", "BAD-NAME", "NON_ASCII_界"] {
        assert_eq!(
            EnvironmentVariableName::new(invalid)
                .err()
                .map(|error| error.kind()),
            Some(ExecutionEnvironmentErrorKind::InvalidEnvironmentVariable)
        );
    }
    assert!(EnvironmentVariableName::new("_TOKEN").is_ok());
    assert_eq!(
        EnvironmentVariableName::new("X".repeat(MAX_ENVIRONMENT_NAME_BYTES + 1))
            .err()
            .map(|error| error.kind()),
        Some(ExecutionEnvironmentErrorKind::InvalidEnvironmentVariable)
    );
    assert_eq!(
        DelegationSourceRevision::new(0)
            .err()
            .map(|error| error.kind()),
        Some(ExecutionEnvironmentErrorKind::InvalidRevision)
    );
    for invalid in ["", "Uppercase", "-leading", "secret=value"] {
        assert!(CredentialSlotId::new(invalid).is_err());
        assert!(DelegationSourceAuthorityId::new(invalid).is_err());
        assert!(DelegatedAuthorityId::new(invalid).is_err());
    }

    let duplicate_environment = ExecutionEnvironmentConfig::new(
        ExecutionEnvironmentLimits::default(),
        [
            environment_delegation("PATH", "source.environment.first", 1)?,
            environment_delegation("PATH", "source.environment.second", 2)?,
        ],
        [],
    );
    assert_eq!(
        duplicate_environment.err().map(|error| error.kind()),
        Some(ExecutionEnvironmentErrorKind::DuplicateAllowlistEntry)
    );
    let duplicate_credentials = ExecutionEnvironmentConfig::new(
        ExecutionEnvironmentLimits::default(),
        [],
        [
            credential_delegation(
                "credential.primary/default",
                "source.credential.first",
                1,
                "authority.account-a.access-standard.billing-included",
            )?,
            credential_delegation(
                "credential.primary/default",
                "source.credential.second",
                2,
                "authority.account-b.access-standard.billing-metered",
            )?,
        ],
    );
    assert_eq!(
        duplicate_credentials.err().map(|error| error.kind()),
        Some(ExecutionEnvironmentErrorKind::DuplicateAllowlistEntry)
    );

    let too_many = (0..=MAX_ENVIRONMENT_ALLOWLIST_ENTRIES)
        .map(|index| {
            environment_delegation(
                &format!("VARIABLE_{index}"),
                "source.environment.default",
                1,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        ExecutionEnvironmentConfig::new(ExecutionEnvironmentLimits::default(), too_many, [],)
            .err()
            .map(|error| error.kind()),
        Some(ExecutionEnvironmentErrorKind::InvalidLimit)
    );
    Ok(())
}

#[test]
fn constructors_reject_invalid_output_and_deadline_limits() {
    let invalid = [
        ExecutionEnvironmentLimits::new(0, 1, 1, 1, Duration::from_secs(1), Duration::from_secs(1)),
        ExecutionEnvironmentLimits::new(2, 2, 1, 1, Duration::from_secs(1), Duration::from_secs(1)),
        ExecutionEnvironmentLimits::new(
            1,
            1,
            2,
            MAX_ENVIRONMENT_OUTPUT_BYTES + 1,
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
        ExecutionEnvironmentLimits::new(1, 1, 2, 1, Duration::ZERO, Duration::from_secs(1)),
        ExecutionEnvironmentLimits::new(
            1,
            1,
            2,
            1,
            MAX_ENVIRONMENT_EXECUTION_DURATION + Duration::from_nanos(1),
            Duration::from_secs(1),
        ),
        ExecutionEnvironmentLimits::new(
            1,
            1,
            2,
            1,
            Duration::from_secs(1),
            MAX_ENVIRONMENT_CLEANUP_DURATION + Duration::from_nanos(1),
        ),
    ];
    assert!(invalid.iter().all(|result| {
        result.as_ref().err().map(ExecutionEnvironmentError::kind)
            == Some(ExecutionEnvironmentErrorKind::InvalidLimit)
    }));
}

#[test]
fn debug_redacts_paths_and_delegated_authority_identifiers() -> Result<(), TestError> {
    let workspace = tempfile::Builder::new()
        .prefix("SECRET_PATH_SENTINEL")
        .tempdir()?;
    let environment_binding = environment_delegation(
        "SECRET_VARIABLE_SENTINEL",
        "secret.environment.source.sentinel",
        1,
    )?;
    let credential_binding = credential_delegation(
        "secret.credential.slot.sentinel",
        "secret.credential.source.sentinel",
        1,
        "secret.delegated.authority.sentinel",
    )?;
    let config = ExecutionEnvironmentConfig::new(
        ExecutionEnvironmentLimits::default(),
        [host_environment_delegation("SECRET_VARIABLE_SENTINEL")?],
        [credential_binding.clone()],
    )?;
    let environment = host(
        workspace.path(),
        ".",
        ExecutionEnvironmentScope::Run(run_id(10)?),
        config,
    )?;
    let debug = format!("{environment:?} {environment_binding:?} {credential_binding:?}");
    for sentinel in [
        "SECRET_PATH_SENTINEL",
        "SECRET_VARIABLE_SENTINEL",
        "secret.environment.source.sentinel",
        "secret.credential.slot.sentinel",
        "secret.credential.source.sentinel",
        "secret.delegated.authority.sentinel",
    ] {
        assert!(!debug.contains(sentinel));
    }
    Ok(())
}

#[test]
fn workspace_rejects_traversal_missing_cwd_and_symlink_escape() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let scope = ExecutionEnvironmentScope::Run(run_id(11)?);
    for cwd in ["../outside", "/absolute", "missing"] {
        assert_eq!(
            host(workspace.path(), cwd, scope, config()?)
                .err()
                .map(|error| error.kind()),
            Some(ExecutionEnvironmentErrorKind::Workspace)
        );
    }
    {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir()?;
        symlink(outside.path(), workspace.path().join("escape"))?;
        assert_eq!(
            host(workspace.path(), "escape", scope, config()?)
                .err()
                .map(|error| error.kind()),
            Some(ExecutionEnvironmentErrorKind::Workspace)
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_canonical_paths_fail_closed() -> Result<(), TestError> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let parent = tempfile::tempdir()?;
    let root = parent.path().join(OsStr::from_bytes(b"workspace-\xff"));
    fs::create_dir(&root)?;
    assert_eq!(
        host(
            &root,
            ".",
            ExecutionEnvironmentScope::Run(run_id(12)?),
            config()?,
        )
        .err()
        .map(|error| error.kind()),
        Some(ExecutionEnvironmentErrorKind::Workspace)
    );
    Ok(())
}

#[test]
fn public_environment_values_preserve_send_sync_contracts() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<ExecutionEnvironment>();
    assert_send_sync::<ExecutionEnvironmentConfig>();
    assert_send_sync::<ExecutionEnvironmentDescriptor>();
    assert_send_sync::<ExecutionEnvironmentLimits>();
    assert_send_sync::<EnvironmentDelegation>();
    assert_send_sync::<CredentialDelegation>();
}

#[test]
fn host_process_prefix_is_reconstructive() -> Result<(), TestError> {
    use plakous_tools::{HostProcessOptions, WorkspacePolicy};
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("first-prefix"))?;
    fs::create_dir(workspace.path().join("second-prefix"))?;
    let make = |prefix: &str| {
        ExecutionEnvironment::host_with_policy_and_process_options(
            ExecutionEnvironmentScope::Run(run_id(99)?),
            WorkspacePolicy::host(workspace.path(), ".")?,
            ExecutionEnvironmentConfig::default(),
            HostProcessOptions::new(Some(prefix.into()))?,
        )
        .map_err(Into::<TestError>::into)
    };
    let first = make("first-prefix")?;
    let fresh = make("first-prefix")?;
    let changed = make("second-prefix")?;
    assert_eq!(first.descriptor().digest(), fresh.descriptor().digest());
    assert_ne!(first.descriptor().digest(), changed.descriptor().digest());
    assert!(!changed.matches_descriptor(first.descriptor()));
    let encoded = serde_json::to_string(first.descriptor())?;
    assert!(encoded.contains("first-prefix"));
    let decoded: ExecutionEnvironmentDescriptor = serde_json::from_str(&encoded)?;
    assert!(first.matches_descriptor(&decoded));
    let rebuilt = ExecutionEnvironment::host_with_policy(
        ExecutionEnvironmentScope::Run(run_id(99)?),
        WorkspacePolicy::host(workspace.path(), ".")?,
        decoded.config().clone(),
    )?;
    assert!(rebuilt.matches_descriptor(&decoded));
    for prefix in ["../escape", "/absolute", "one:two", "", "."] {
        assert!(HostProcessOptions::new(Some(prefix.into())).is_err());
    }
    let colon_root = workspace.path().join("colon:workspace");
    fs::create_dir_all(colon_root.join("prefix"))?;
    assert!(
        ExecutionEnvironment::host_with_policy_and_process_options(
            ExecutionEnvironmentScope::Run(run_id(99)?),
            WorkspacePolicy::host(&colon_root, ".")?,
            ExecutionEnvironmentConfig::default(),
            HostProcessOptions::new(Some("prefix".into()))?,
        )
        .is_err()
    );
    Ok(())
}
