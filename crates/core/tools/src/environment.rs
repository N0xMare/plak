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
mod read_observations;

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
    ProcessContainment, RunnerId,
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

pub(crate) use backend::runner;
pub(crate) use backend::workspace_path_not_found;
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
mod tests;
