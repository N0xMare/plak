//! Immutable execution-environment identity, authority, and bounded effects.
//!
//! Filesystem and process operations cross a sealed, object-safe backend owned
//! by the exact live environment. Network and destructive-action authorization
//! remains the responsibility of `ToolPolicy`; physical containment metadata is
//! descriptive, not a grant.
//! The built-in host backend is allocation-local and assumes the embedding
//! process does not mutate its OS principal or namespaces while that allocation
//! is live. A changed host authority requires a new backend/environment.

use std::fmt;
use std::sync::Arc;

use plakous_model::SafeDiagnostic;
#[cfg(test)]
use plakous_model::{DigestDomain, RunId};

use crate::WorkspacePolicy;

mod backend;
mod config;
mod identity;
mod limits;
mod process_options;

pub use config::{
    CredentialDelegation, CredentialSlotId, DelegatedAuthorityId, DelegationSourceAuthorityId,
    DelegationSourceRevision, EnvironmentDelegation, EnvironmentVariableName, ExecutionEnvironment,
    ExecutionEnvironmentConfig, ExecutionEnvironmentDescriptor, ExecutionEnvironmentScope,
};
pub use identity::{
    EXECUTION_ENVIRONMENT_DIGEST_VERSION, ExecutionBackendAuthorityId,
    ExecutionBackendAuthorityRevision, ExecutionBackendDescriptor, ExecutionBackendIdentity,
    ExecutionBackendKind, ExecutionBackendSemanticRevision, ExecutionContainmentDescriptor,
    FilesystemContainment, HostFilesystemObjectIdentity, HostPathFormat, NetworkContainment,
    ProcessContainment,
};
use identity::{
    HOST_SNAPSHOT_SOURCE_AUTHORITY, HOST_SNAPSHOT_SOURCE_REVISION, environment_digest,
    filesystem_object_identity, has_supported_digest_format, is_lowercase_identifier,
    path_identity_bytes, redacted_identifier_debug,
};
use limits::ExecutionEnvironmentLimitsWire;
pub use limits::{
    ExecutionEnvironmentLimits, MAX_ENVIRONMENT_ALLOWLIST_ENTRIES,
    MAX_ENVIRONMENT_CLEANUP_DURATION, MAX_ENVIRONMENT_EXECUTION_DURATION,
    MAX_ENVIRONMENT_NAME_BYTES, MAX_ENVIRONMENT_OUTPUT_BYTES,
};
pub use process_options::HostProcessOptions;

pub use backend::*;

/// Stable category for execution-environment construction failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutionEnvironmentErrorKind {
    /// An identity or authority identifier was invalid.
    InvalidIdentity,
    /// A semantic or delegation-source revision was invalid.
    InvalidRevision,
    /// An environment-variable name was invalid.
    InvalidEnvironmentVariable,
    /// A credential-slot identifier was invalid.
    InvalidCredentialSlot,
    /// A delegation source-authority identifier was invalid.
    InvalidDelegationSourceAuthority,
    /// A delegated credential-authority identifier was invalid.
    InvalidDelegatedAuthority,
    /// A delegation list contained a duplicate key.
    DuplicateAllowlistEntry,
    /// An output, deadline, or list limit was invalid.
    InvalidLimit,
    /// The workspace could not be canonicalized or contained.
    Workspace,
    /// A persisted or decision-bound identity does not match the live environment.
    IdentityMismatch,
}

/// Bounded, secret-safe error returned while freezing or verifying an environment.
#[derive(Clone, Eq, PartialEq)]
pub struct ExecutionEnvironmentError {
    kind: ExecutionEnvironmentErrorKind,
    message: SafeDiagnostic,
}

impl ExecutionEnvironmentError {
    pub(crate) fn new(kind: ExecutionEnvironmentErrorKind, message: &'static str) -> Self {
        Self {
            kind,
            message: SafeDiagnostic::new(message),
        }
    }

    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> ExecutionEnvironmentErrorKind {
        self.kind
    }

    /// Returns the bounded, sanitized diagnostic.
    #[must_use]
    pub const fn message(&self) -> &SafeDiagnostic {
        &self.message
    }
}

impl fmt::Debug for ExecutionEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionEnvironmentError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .finish()
    }
}

impl fmt::Display for ExecutionEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ExecutionEnvironmentError {}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn source_authority(
        id: &str,
    ) -> Result<DelegationSourceAuthorityId, ExecutionEnvironmentError> {
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
                    DelegatedAuthorityId::new(
                        "authority.account-a.access-standard.billing-included",
                    )?,
                ),
                CredentialDelegation::new(
                    CredentialSlotId::new("credential.secondary/default")?,
                    source_authority("source.credential.fixture")?,
                    DelegationSourceRevision::new(5)?,
                    DelegatedAuthorityId::new(
                        "authority.account-b.access-standard.billing-metered",
                    )?,
                ),
            ],
        )?;
        let backend =
            ExecutionBackendDescriptor::host_for_platform("linux", HostPathFormat::UnixUtf8);
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
    fn generation_stamp_is_excluded_from_environment_digest()
    -> Result<(), Box<dyn std::error::Error>> {
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
}
