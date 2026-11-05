//! Environment configuration, delegation, and descriptor wire forms.

use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::Duration;

use plakous_model::{AttemptId, Digest, RunId};
use serde::{Deserialize, Deserializer, Serialize};

use crate::IsolationLevel;

use super::backend::{
    DeterministicExecutionBackend, ExecutionBackend, HostProcessContext, JoinedProcessServiceCache,
    LineOwner, host_execution_backend,
};
use super::{
    Arc, ExecutionBackendDescriptor, ExecutionBackendIdentity, ExecutionEnvironmentError,
    ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits, ExecutionEnvironmentLimitsWire,
    HOST_SNAPSHOT_SOURCE_AUTHORITY, HOST_SNAPSHOT_SOURCE_REVISION, HostFilesystemObjectIdentity,
    HostProcessOptions, MAX_ENVIRONMENT_ALLOWLIST_ENTRIES, MAX_ENVIRONMENT_NAME_BYTES,
    WorkspacePolicy, environment_digest, filesystem_object_identity, has_supported_digest_format,
    is_lowercase_identifier, path_identity_bytes, process_options, redacted_identifier_debug,
};

/// Lifetime boundary within which an environment identity may be reused.
///
/// Scope is identity metadata, not authorization. A future decision must still
/// bind the exact invocation and complete environment digest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ExecutionEnvironmentScope {
    /// The environment may be reused only within this run.
    Run(RunId),
    /// The environment may be reused only by this attempt in this run.
    Attempt {
        /// Owning run identity.
        run_id: RunId,
        /// Owning attempt identity.
        attempt_id: AttemptId,
    },
}

impl ExecutionEnvironmentScope {
    /// Returns the run to which the environment is scoped.
    #[must_use]
    pub const fn run_id(self) -> RunId {
        match self {
            Self::Run(run_id) | Self::Attempt { run_id, .. } => run_id,
        }
    }

    /// Returns the narrower attempt scope, when present.
    #[must_use]
    pub const fn attempt_id(self) -> Option<AttemptId> {
        match self {
            Self::Run(_) => None,
            Self::Attempt { attempt_id, .. } => Some(attempt_id),
        }
    }
}

/// Canonical uppercase environment-variable name for explicit delegation.
///
/// This type identifies a name only. It cannot carry or capture an ambient
/// value.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EnvironmentVariableName(Arc<str>);

impl EnvironmentVariableName {
    /// Validates `[A-Z_][A-Z0-9_]*` environment-variable text.
    ///
    /// # Errors
    ///
    /// Returns an invalid-environment-variable error for mixed/lowercase,
    /// oversized, empty, or otherwise non-portable text.
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self, ExecutionEnvironmentError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let valid_first = bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_');
        let valid = valid_first
            && value.len() <= MAX_ENVIRONMENT_NAME_BYTES
            && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
        if !valid {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidEnvironmentVariable,
                "environment-variable name is not canonical uppercase ASCII",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the canonical name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EnvironmentVariableName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        redacted_identifier_debug("EnvironmentVariableName", self.0.len(), formatter)
    }
}

macro_rules! lowercase_identifier {
    ($name:ident, $kind:expr, $message:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Creates a validated lowercase portable identifier.
            ///
            /// # Errors
            ///
            /// Returns a typed environment error for invalid text.
            pub fn new(value: impl Into<Arc<str>>) -> Result<Self, ExecutionEnvironmentError> {
                let value = value.into();
                if !is_lowercase_identifier(&value) {
                    return Err(ExecutionEnvironmentError::new($kind, $message));
                }
                Ok(Self(value))
            }

            /// Returns the validated non-secret identifier.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                redacted_identifier_debug(stringify!($name), self.0.len(), formatter)
            }
        }
    };
}

lowercase_identifier!(
    CredentialSlotId,
    ExecutionEnvironmentErrorKind::InvalidCredentialSlot,
    "credential-slot ID is not a portable lowercase identifier",
    "Validated non-secret identifier for a credential resolver slot."
);
lowercase_identifier!(
    DelegationSourceAuthorityId,
    ExecutionEnvironmentErrorKind::InvalidDelegationSourceAuthority,
    "delegation source-authority ID is not a portable lowercase identifier",
    "Validated non-secret authority ID of an environment or credential delegation source."
);
lowercase_identifier!(
    DelegatedAuthorityId,
    ExecutionEnvironmentErrorKind::InvalidDelegatedAuthority,
    "delegated-authority ID is not a portable lowercase identifier",
    "Opaque provider-neutral credential authority ID binding account, access, and billing scope."
);

/// Nonzero semantic revision of a delegation source.
///
/// A resolver must advance this revision whenever its effective value or
/// credential authority mapping changes.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DelegationSourceRevision(NonZeroU64);

impl DelegationSourceRevision {
    /// Creates a nonzero source revision.
    ///
    /// # Errors
    ///
    /// Returns an invalid-revision error for zero.
    pub fn new(value: u64) -> Result<Self, ExecutionEnvironmentError> {
        NonZeroU64::new(value).map(Self).ok_or_else(|| {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidRevision,
                "delegation-source revision must be nonzero",
            )
        })
    }

    /// Returns the numeric source revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Debug for DelegationSourceRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DelegationSourceRevision")
            .field(&self.get())
            .finish()
    }
}

/// Exact environment-variable name and source-authority delegation.
///
/// A future resolver must verify both the source-authority ID and semantic
/// revision before resolving the named value. This binding contains no value.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EnvironmentDelegation {
    pub(super) name: EnvironmentVariableName,
    pub(super) source_authority_id: DelegationSourceAuthorityId,
    pub(super) source_revision: DelegationSourceRevision,
}

impl EnvironmentDelegation {
    /// Creates an immutable environment-variable delegation binding.
    #[must_use]
    pub const fn new(
        name: EnvironmentVariableName,
        source_authority_id: DelegationSourceAuthorityId,
        source_revision: DelegationSourceRevision,
    ) -> Self {
        Self {
            name,
            source_authority_id,
            source_revision,
        }
    }

    /// Creates a binding to the host backend's construction-time environment
    /// snapshot.
    ///
    /// The host backend captures the named value once while the execution
    /// environment is built. It never rereads mutable ambient process state for
    /// a later dispatch. An absent value remains absent from child processes.
    ///
    /// # Errors
    ///
    /// Returns an identity error only if the built-in source identity is
    /// internally invalid.
    pub fn host_snapshot(name: EnvironmentVariableName) -> Result<Self, ExecutionEnvironmentError> {
        Ok(Self::new(
            name,
            DelegationSourceAuthorityId::new(HOST_SNAPSHOT_SOURCE_AUTHORITY)?,
            DelegationSourceRevision::new(HOST_SNAPSHOT_SOURCE_REVISION)?,
        ))
    }

    /// Returns the canonical delegated variable name.
    #[must_use]
    pub const fn name(&self) -> &EnvironmentVariableName {
        &self.name
    }

    /// Returns the exact resolver source-authority ID.
    #[must_use]
    pub const fn source_authority_id(&self) -> &DelegationSourceAuthorityId {
        &self.source_authority_id
    }

    /// Returns the resolver semantic revision.
    #[must_use]
    pub const fn source_revision(&self) -> DelegationSourceRevision {
        self.source_revision
    }

    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "read only by the unix host backend")
    )]
    pub(super) fn is_host_snapshot(&self) -> bool {
        self.source_authority_id.as_str() == HOST_SNAPSHOT_SOURCE_AUTHORITY
            && self.source_revision.get() == HOST_SNAPSHOT_SOURCE_REVISION
    }
}

impl fmt::Debug for EnvironmentDelegation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvironmentDelegation")
            .field("name", &"<redacted>")
            .field("source_authority_id", &"<redacted>")
            .field("source_revision", &self.source_revision)
            .finish()
    }
}

/// Exact credential slot, resolver source, and authority delegation.
///
/// [`DelegatedAuthorityId`] is one provider-neutral, non-secret, opaque identity
/// that the resolver defines to bind account, access/audience, and billing
/// scope. A future resolver must verify the slot, source-authority ID/revision,
/// and delegated-authority ID before returning secret material; this binding
/// never contains that material.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialDelegation {
    pub(super) slot: CredentialSlotId,
    pub(super) source_authority_id: DelegationSourceAuthorityId,
    pub(super) source_revision: DelegationSourceRevision,
    pub(super) delegated_authority_id: DelegatedAuthorityId,
}

impl CredentialDelegation {
    /// Creates an immutable credential delegation binding.
    #[must_use]
    pub const fn new(
        slot: CredentialSlotId,
        source_authority_id: DelegationSourceAuthorityId,
        source_revision: DelegationSourceRevision,
        delegated_authority_id: DelegatedAuthorityId,
    ) -> Self {
        Self {
            slot,
            source_authority_id,
            source_revision,
            delegated_authority_id,
        }
    }

    /// Returns the delegated credential slot.
    #[must_use]
    pub const fn slot(&self) -> &CredentialSlotId {
        &self.slot
    }

    /// Returns the exact resolver source-authority ID.
    #[must_use]
    pub const fn source_authority_id(&self) -> &DelegationSourceAuthorityId {
        &self.source_authority_id
    }

    /// Returns the resolver semantic revision.
    #[must_use]
    pub const fn source_revision(&self) -> DelegationSourceRevision {
        self.source_revision
    }

    /// Returns the opaque account/access/billing authority identity.
    #[must_use]
    pub const fn delegated_authority_id(&self) -> &DelegatedAuthorityId {
        &self.delegated_authority_id
    }
}

impl fmt::Debug for CredentialDelegation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialDelegation")
            .field("slot", &"<redacted>")
            .field("source_authority_id", &"<redacted>")
            .field("source_revision", &self.source_revision)
            .field("delegated_authority_id", &"<redacted>")
            .finish()
    }
}

/// Immutable deterministic environment authority configuration.
///
/// Delegation lists carry names plus exact non-secret resolver identities and
/// revisions. Values and credential material are deliberately absent, and both
/// lists default to empty.
#[derive(Clone, Eq, PartialEq)]
pub struct ExecutionEnvironmentConfig {
    pub(super) limits: ExecutionEnvironmentLimits,
    pub(super) environment_delegations: Arc<[EnvironmentDelegation]>,
    pub(super) credential_delegations: Arc<[CredentialDelegation]>,
    pub(super) required_path_prefix: Option<String>,
    pub(super) discovery_ceiling: bool,
}

impl ExecutionEnvironmentConfig {
    /// Freezes limits and delegation bindings in canonical key order.
    ///
    /// # Errors
    ///
    /// Returns a typed error for duplicate variable names/credential slots or
    /// when either bounded list exceeds its hard entry cap.
    pub fn new(
        limits: ExecutionEnvironmentLimits,
        environment_delegations: impl IntoIterator<Item = EnvironmentDelegation>,
        credential_delegations: impl IntoIterator<Item = CredentialDelegation>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let mut environment_delegations = collect_bounded(environment_delegations)?;
        environment_delegations.sort_by(|left, right| left.name.cmp(&right.name));
        if environment_delegations
            .windows(2)
            .any(|pair| pair[0].name == pair[1].name)
        {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::DuplicateAllowlistEntry,
                "environment delegation list contains a duplicate name",
            ));
        }

        let mut credential_delegations = collect_bounded(credential_delegations)?;
        credential_delegations.sort_by(|left, right| left.slot.cmp(&right.slot));
        if credential_delegations
            .windows(2)
            .any(|pair| pair[0].slot == pair[1].slot)
        {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::DuplicateAllowlistEntry,
                "credential delegation list contains a duplicate slot",
            ));
        }

        Ok(Self {
            limits,
            environment_delegations: environment_delegations.into(),
            credential_delegations: credential_delegations.into(),
            required_path_prefix: None,
            discovery_ceiling: false,
        })
    }

    /// Returns hard output and deadline limits.
    #[must_use]
    pub const fn limits(&self) -> ExecutionEnvironmentLimits {
        self.limits
    }

    /// Returns canonical environment-variable delegation bindings.
    #[must_use]
    pub fn environment_delegations(&self) -> &[EnvironmentDelegation] {
        &self.environment_delegations
    }

    /// Returns canonical credential delegation bindings.
    #[must_use]
    pub fn credential_delegations(&self) -> &[CredentialDelegation] {
        &self.credential_delegations
    }
}

impl Default for ExecutionEnvironmentConfig {
    fn default() -> Self {
        Self {
            limits: ExecutionEnvironmentLimits::default(),
            environment_delegations: Arc::from([]),
            credential_delegations: Arc::from([]),
            required_path_prefix: None,
            discovery_ceiling: false,
        }
    }
}

impl fmt::Debug for ExecutionEnvironmentConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionEnvironmentConfig")
            .field("limits", &self.limits)
            .field("required_path_prefix", &self.required_path_prefix.is_some())
            .field("discovery_ceiling", &self.discovery_ceiling)
            .field(
                "environment_delegation_count",
                &self.environment_delegations.len(),
            )
            .field("environment_delegations", &"<redacted>")
            .field(
                "credential_delegation_count",
                &self.credential_delegations.len(),
            )
            .field("credential_delegations", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EnvironmentDelegationWire {
    pub(super) name: String,
    pub(super) source_authority_id: String,
    pub(super) source_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialDelegationWire {
    pub(super) slot: String,
    pub(super) source_authority_id: String,
    pub(super) source_revision: u64,
    pub(super) delegated_authority_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionEnvironmentDescriptorWire {
    pub(super) backend: ExecutionBackendDescriptor,
    pub(super) scope: ExecutionEnvironmentScope,
    pub(super) canonical_root: String,
    pub(super) canonical_cwd: String,
    pub(super) root_object: HostFilesystemObjectIdentity,
    pub(super) limits: ExecutionEnvironmentLimitsWire,
    pub(super) environment_delegations: Vec<EnvironmentDelegationWire>,
    pub(super) credential_delegations: Vec<CredentialDelegationWire>,
    pub(super) required_path_prefix: Option<String>,
    pub(super) discovery_ceiling: bool,
    pub(super) digest: Digest,
}

/// Serializable reconstruction-complete semantics of one execution environment.
///
/// The descriptor deliberately excludes allocation-local backend capabilities
/// and mutable workspace contents. Exact reopen compares canonical paths, the
/// stable root filesystem object, backend semantics, scope, limits, and every
/// delegation identity before a fresh live dispatch fence can be minted.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(into = "ExecutionEnvironmentDescriptorWire")]
pub struct ExecutionEnvironmentDescriptor {
    pub(super) backend: ExecutionBackendDescriptor,
    pub(super) scope: ExecutionEnvironmentScope,
    pub(super) canonical_root: Arc<str>,
    pub(super) canonical_cwd: Arc<str>,
    pub(super) root_object: HostFilesystemObjectIdentity,
    pub(super) config: Arc<ExecutionEnvironmentConfig>,
    pub(super) digest: Digest,
}

impl From<ExecutionEnvironmentDescriptor> for ExecutionEnvironmentDescriptorWire {
    fn from(descriptor: ExecutionEnvironmentDescriptor) -> Self {
        let limits = descriptor.config.limits();
        Self {
            backend: descriptor.backend,
            scope: descriptor.scope,
            canonical_root: descriptor.canonical_root.to_string(),
            canonical_cwd: descriptor.canonical_cwd.to_string(),
            root_object: descriptor.root_object,
            limits: ExecutionEnvironmentLimitsWire {
                max_stdout_bytes: limits.max_stdout_bytes(),
                max_stderr_bytes: limits.max_stderr_bytes(),
                max_combined_output_bytes: limits.max_combined_output_bytes(),
                max_artifact_bytes: limits.max_artifact_bytes(),
                execution_seconds: limits.execution_duration().as_secs(),
                execution_nanoseconds: limits.execution_duration().subsec_nanos(),
                session_seconds: limits.session_duration().as_secs(),
                foreground_seconds: limits.foreground_seconds(),
                session_nanoseconds: limits.session_duration().subsec_nanos(),
                cleanup_seconds: limits.cleanup_duration().as_secs(),
                cleanup_nanoseconds: limits.cleanup_duration().subsec_nanos(),
            },
            environment_delegations: descriptor
                .config
                .environment_delegations()
                .iter()
                .map(|binding| EnvironmentDelegationWire {
                    name: binding.name().as_str().to_owned(),
                    source_authority_id: binding.source_authority_id().as_str().to_owned(),
                    source_revision: binding.source_revision().get(),
                })
                .collect(),
            credential_delegations: descriptor
                .config
                .credential_delegations()
                .iter()
                .map(|binding| CredentialDelegationWire {
                    slot: binding.slot().as_str().to_owned(),
                    source_authority_id: binding.source_authority_id().as_str().to_owned(),
                    source_revision: binding.source_revision().get(),
                    delegated_authority_id: binding.delegated_authority_id().as_str().to_owned(),
                })
                .collect(),
            required_path_prefix: descriptor.config.required_path_prefix.clone(),
            discovery_ceiling: descriptor.config.discovery_ceiling,
            digest: descriptor.digest,
        }
    }
}

impl<'de> Deserialize<'de> for ExecutionEnvironmentDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExecutionEnvironmentDescriptorWire::deserialize(deserializer)?;
        let config = config_from_wire(&wire).map_err(serde::de::Error::custom)?;
        let expected = environment_digest(
            &wire.backend,
            wire.scope,
            wire.canonical_root.as_bytes(),
            wire.canonical_cwd.as_bytes(),
            wire.root_object,
            &config,
        )
        .map_err(serde::de::Error::custom)?;
        if !has_supported_digest_format(wire.digest) || expected != wire.digest {
            return Err(serde::de::Error::custom(
                "execution environment descriptor has an invalid digest",
            ));
        }
        Ok(Self {
            backend: wire.backend,
            scope: wire.scope,
            canonical_root: wire.canonical_root.into(),
            canonical_cwd: wire.canonical_cwd.into(),
            root_object: wire.root_object,
            config: Arc::new(config),
            digest: wire.digest,
        })
    }
}

impl ExecutionEnvironmentDescriptor {
    /// Returns the exact backend semantics and containment.
    #[must_use]
    pub const fn backend(&self) -> &ExecutionBackendDescriptor {
        &self.backend
    }

    /// Returns the explicit reuse scope.
    #[must_use]
    pub const fn scope(&self) -> ExecutionEnvironmentScope {
        self.scope
    }

    /// Returns the canonical UTF-8 workspace root identity.
    #[must_use]
    pub fn canonical_root(&self) -> &str {
        &self.canonical_root
    }

    /// Returns the canonical UTF-8 workspace current-directory identity.
    #[must_use]
    pub fn canonical_cwd(&self) -> &str {
        &self.canonical_cwd
    }

    /// Returns the stable filesystem object captured for the root.
    #[must_use]
    pub const fn root_object(&self) -> HostFilesystemObjectIdentity {
        self.root_object
    }

    /// Returns the complete stable limits and delegation configuration.
    #[must_use]
    pub fn config(&self) -> &ExecutionEnvironmentConfig {
        &self.config
    }

    /// Returns the complete authority-affecting digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

impl fmt::Debug for ExecutionEnvironmentDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionEnvironmentDescriptor")
            .field("backend", &self.backend)
            .field("scope", &self.scope)
            .field("canonical_root", &"<redacted>")
            .field("canonical_cwd", &"<redacted>")
            .field("root_object", &self.root_object)
            .field("config", &self.config)
            .field("digest", &self.digest)
            .finish()
    }
}

/// Frozen execution-environment identity, authority, and operation backend.
///
/// Construction canonicalizes root/cwd through [`WorkspacePolicy`] and mints a
/// backend-allocation identity. Host path checks do not confine process tools.
#[derive(Clone)]
pub struct ExecutionEnvironment {
    pub(super) descriptor: ExecutionEnvironmentDescriptor,
    pub(super) live_backend: ExecutionBackendIdentity,
    pub(super) workspace: Arc<WorkspacePolicy>,
    pub(super) relative_cwd: Arc<PathBuf>,
    pub(super) config: Arc<ExecutionEnvironmentConfig>,
    pub(super) operation_backend: Arc<dyn ExecutionBackend>,
    pub(super) joined_process_sessions: Arc<JoinedProcessServiceCache>,
    pub(super) host_process: Option<Arc<HostProcessContext>>,
    pub(super) read_observations: Arc<super::read_observations::ReadObservations>,
    /// Live image-read facts. Absent from the descriptor digest.
    pub(super) visual_read: Option<Arc<crate::internal::VisualReadSession>>,
    /// The line head this environment's operations run for. Process-local
    /// and absent from the descriptor digest: a reopen owns under a new fence.
    pub(super) line_owner: Option<LineOwner>,
    /// The link to the runner this environment's operations run on, when it
    /// is not the local runner. Process-local, like the line owner.
    pub(super) runner_link: Option<Arc<dyn crate::environment::runner::RunnerLink>>,
    /// The home-local runner this local environment's process sessions run
    /// on, when one carries them. Process-local, like the line owner.
    pub(super) local_daemon: Option<crate::environment::runner::LocalDaemon>,
}

impl ExecutionEnvironment {
    /// Freezes one host environment after canonical workspace validation.
    ///
    /// The backend identity, semantic revision, OS/path tag, and physical
    /// containment descriptor are minted internally. `cwd` is relative to `root`
    /// and must exist. Canonical paths must be UTF-8 for explicit identity bytes.
    ///
    /// # Errors
    ///
    /// Returns a workspace error for I/O, traversal, escape, or non-UTF-8 paths.
    pub fn host(
        scope: ExecutionEnvironmentScope,
        root: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
        config: ExecutionEnvironmentConfig,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let workspace = WorkspacePolicy::host(root, cwd).map_err(|_| {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::Workspace,
                "execution workspace could not be canonicalized or contained",
            )
        })?;
        Self::host_with_policy(scope, workspace, config)
    }

    /// Freezes a host environment from an already-canonical workspace policy.
    ///
    /// Observational generation stamps on `workspace` are preserved and are not
    /// part of execution-environment identity.
    ///
    /// # Errors
    ///
    /// Returns a workspace error for non-UTF-8 paths or backend construction.
    pub fn host_with_policy(
        scope: ExecutionEnvironmentScope,
        workspace: WorkspacePolicy,
        config: ExecutionEnvironmentConfig,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let options = HostProcessOptions {
            required_path_prefix: config.required_path_prefix.clone(),
            discovery_ceiling: config.discovery_ceiling,
        };
        Self::host_with_policy_and_process_options(scope, workspace, config, options)
    }

    /// Constructs host authority with an explicit required process prefix.
    /// Ambient PATH/HOME are never durable descriptor material.
    ///
    /// # Errors
    /// Rejects unavailable or escaping prefixes and invalid host authority.
    pub fn host_with_policy_and_process_options(
        scope: ExecutionEnvironmentScope,
        workspace: WorkspacePolicy,
        mut config: ExecutionEnvironmentConfig,
        options: HostProcessOptions,
    ) -> Result<Self, ExecutionEnvironmentError> {
        config
            .required_path_prefix
            .clone_from(&options.required_path_prefix);
        config.discovery_ceiling = options.discovery_ceiling;
        let process = Arc::new(HostProcessContext::capture(&workspace, options)?);
        let backend = host_execution_backend(&config, &process)?;
        let mut environment = Self::from_workspace_and_backend(scope, workspace, config, backend)?;
        environment.host_process = Some(process);
        Ok(environment)
    }

    /// Freezes an environment using a deterministic injectable backend.
    ///
    /// The backend simulates process execution and is intended for conformance
    /// tests. Filesystem operations still use the supplied disposable workspace
    /// with truthful host path-policy-only containment.
    ///
    /// # Errors
    ///
    /// Returns a workspace or backend identity error.
    pub fn deterministic(
        scope: ExecutionEnvironmentScope,
        root: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
        config: ExecutionEnvironmentConfig,
        backend: Arc<DeterministicExecutionBackend>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let backend: Arc<dyn ExecutionBackend> = backend;
        Self::with_operation_backend(scope, root, cwd, config, backend)
    }

    pub(super) fn with_operation_backend(
        scope: ExecutionEnvironmentScope,
        root: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
        config: ExecutionEnvironmentConfig,
        operation_backend: Arc<dyn ExecutionBackend>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let workspace = WorkspacePolicy::host(root, cwd).map_err(|_| {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::Workspace,
                "execution workspace could not be canonicalized or contained",
            )
        })?;
        Self::from_workspace_and_backend(scope, workspace, config, operation_backend)
    }

    pub(super) fn from_workspace_and_backend(
        scope: ExecutionEnvironmentScope,
        workspace: WorkspacePolicy,
        config: ExecutionEnvironmentConfig,
        operation_backend: Arc<dyn ExecutionBackend>,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let relative_cwd = workspace
            .cwd()
            .strip_prefix(workspace.root())
            .map_err(|_| {
                ExecutionEnvironmentError::new(
                    ExecutionEnvironmentErrorKind::Workspace,
                    "execution workspace current directory is not contained",
                )
            })?
            .to_path_buf();
        let live_backend = operation_backend.identity().clone();
        let backend = live_backend.descriptor().clone();
        let canonical_root = path_identity_bytes(workspace.root())?;
        let canonical_cwd = path_identity_bytes(workspace.cwd())?;
        let root_object = filesystem_object_identity(workspace.root())?;
        let digest = environment_digest(
            &backend,
            scope,
            &canonical_root,
            &canonical_cwd,
            root_object,
            &config,
        )?;
        Ok(Self {
            descriptor: ExecutionEnvironmentDescriptor {
                backend,
                scope,
                canonical_root: String::from_utf8(canonical_root)
                    .map_err(|_| {
                        ExecutionEnvironmentError::new(
                            ExecutionEnvironmentErrorKind::Workspace,
                            "execution workspace root is not valid UTF-8",
                        )
                    })?
                    .into(),
                canonical_cwd: String::from_utf8(canonical_cwd)
                    .map_err(|_| {
                        ExecutionEnvironmentError::new(
                            ExecutionEnvironmentErrorKind::Workspace,
                            "execution workspace cwd is not valid UTF-8",
                        )
                    })?
                    .into(),
                root_object,
                config: Arc::new(config.clone()),
                digest,
            },
            live_backend,
            workspace: Arc::new(workspace),
            relative_cwd: Arc::new(relative_cwd),
            config: Arc::new(config),
            operation_backend,
            joined_process_sessions: Arc::new(JoinedProcessServiceCache::default()),
            host_process: None,
            read_observations: Arc::default(),
            visual_read: None,
            line_owner: None,
            runner_link: None,
            local_daemon: None,
        })
    }

    /// Names the line head every operation started on this environment runs
    /// for. Carried into each tool call's
    /// [`OperationContext`](crate::OperationContext); not yet
    /// enforced.
    #[must_use]
    pub fn with_line_owner(mut self, owner: LineOwner) -> Self {
        self.line_owner = Some(owner);
        self
    }

    /// The line head this environment's operations run for, when the agent
    /// that built it named one.
    #[must_use]
    pub const fn line_owner(&self) -> Option<LineOwner> {
        self.line_owner
    }

    /// Installs live image-read facts. Not part of the descriptor.
    ///
    /// The agent derives the route facts; a host supplies only the normalizer.
    #[must_use]
    pub(crate) fn with_visual_read(
        mut self,
        session: Arc<crate::internal::VisualReadSession>,
    ) -> Self {
        self.visual_read = Some(session);
        self
    }

    /// The live image-read facts, when the agent installed them.
    pub(crate) fn visual_read(&self) -> Option<&crate::internal::VisualReadSession> {
        self.visual_read.as_deref()
    }

    /// Returns the exact identity to bind into audit and authorization records.
    #[must_use]
    pub const fn descriptor(&self) -> &ExecutionEnvironmentDescriptor {
        &self.descriptor
    }

    /// Returns the backend-owned host identity and containment descriptor.
    #[must_use]
    pub const fn backend(&self) -> &ExecutionBackendIdentity {
        &self.live_backend
    }

    /// Returns the canonical workspace policy.
    #[must_use]
    pub fn workspace(&self) -> &WorkspacePolicy {
        &self.workspace
    }

    /// Returns the canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.workspace.root()
    }

    /// Returns the canonical current directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        self.workspace.cwd()
    }

    /// Returns the canonical current directory relative to the workspace root.
    #[must_use]
    pub fn relative_cwd(&self) -> &Path {
        &self.relative_cwd
    }

    /// Returns the truthful isolation level.
    ///
    /// Both the host and deterministic backend use host workspace paths; the
    /// more precise backend containment descriptor distinguishes their process
    /// and network behavior.
    #[must_use]
    pub fn isolation(&self) -> IsolationLevel {
        self.workspace.isolation()
    }

    /// Returns the frozen authority configuration.
    #[must_use]
    pub fn config(&self) -> &ExecutionEnvironmentConfig {
        &self.config
    }

    /// Returns whether `candidate` exactly matches this live environment.
    ///
    /// The check includes algorithm/domain/version, backend identity and
    /// semantic revision, OS/path format, physical containment descriptor, scope,
    /// canonical paths, limits, and every delegation binding.
    #[must_use]
    pub fn matches_descriptor(&self, candidate: &ExecutionEnvironmentDescriptor) -> bool {
        if !has_supported_digest_format(candidate.digest)
            || candidate.backend != self.descriptor.backend
            || candidate.scope != self.descriptor.scope
        {
            return false;
        }
        self.recompute_digest()
            .is_ok_and(|digest| digest == candidate.digest && digest == self.descriptor.digest)
    }

    /// Verifies an decision/persisted identity against this live environment.
    ///
    /// # Errors
    ///
    /// Returns an identity-mismatch error without exposing configuration data.
    pub fn verify_descriptor(
        &self,
        candidate: &ExecutionEnvironmentDescriptor,
    ) -> Result<(), ExecutionEnvironmentError> {
        if self.matches_descriptor(candidate) {
            Ok(())
        } else {
            Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::IdentityMismatch,
                "execution environment descriptor does not match the live environment",
            ))
        }
    }

    pub(super) fn recompute_digest(&self) -> Result<Digest, ExecutionEnvironmentError> {
        if self.operation_backend.identity() != &self.live_backend {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::IdentityMismatch,
                "execution backend identity changed after environment construction",
            ));
        }
        environment_digest(
            self.live_backend.descriptor(),
            self.descriptor.scope,
            &path_identity_bytes(self.workspace.root())?,
            &path_identity_bytes(self.workspace.cwd())?,
            filesystem_object_identity(self.workspace.root())?,
            &self.config,
        )
    }
}

impl fmt::Debug for ExecutionEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionEnvironment")
            .field("descriptor", &self.descriptor)
            .field("root", &"<redacted>")
            .field("cwd", &"<redacted>")
            .field("isolation", &self.isolation())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn collect_bounded<T>(
    values: impl IntoIterator<Item = T>,
) -> Result<Vec<T>, ExecutionEnvironmentError> {
    let values = values
        .into_iter()
        .take(MAX_ENVIRONMENT_ALLOWLIST_ENTRIES + 1)
        .collect::<Vec<_>>();
    if values.len() > MAX_ENVIRONMENT_ALLOWLIST_ENTRIES {
        return Err(ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::InvalidLimit,
            "environment delegation list exceeds its hard entry limit",
        ));
    }
    Ok(values)
}

fn config_from_wire(
    wire: &ExecutionEnvironmentDescriptorWire,
) -> Result<ExecutionEnvironmentConfig, ExecutionEnvironmentError> {
    let limits = ExecutionEnvironmentLimits::new(
        wire.limits.max_stdout_bytes,
        wire.limits.max_stderr_bytes,
        wire.limits.max_combined_output_bytes,
        wire.limits.max_artifact_bytes,
        Duration::new(
            wire.limits.execution_seconds,
            wire.limits.execution_nanoseconds,
        ),
        Duration::new(wire.limits.cleanup_seconds, wire.limits.cleanup_nanoseconds),
    )?
    .with_session_duration(Duration::new(
        wire.limits.session_seconds,
        wire.limits.session_nanoseconds,
    ))?
    .with_foreground_seconds(wire.limits.foreground_seconds)?;
    let environment_delegations = wire
        .environment_delegations
        .iter()
        .map(|binding| {
            Ok(EnvironmentDelegation::new(
                EnvironmentVariableName::new(binding.name.clone())?,
                DelegationSourceAuthorityId::new(binding.source_authority_id.clone())?,
                DelegationSourceRevision::new(binding.source_revision)?,
            ))
        })
        .collect::<Result<Vec<_>, ExecutionEnvironmentError>>()?;
    let credential_delegations = wire
        .credential_delegations
        .iter()
        .map(|binding| {
            Ok(CredentialDelegation::new(
                CredentialSlotId::new(binding.slot.clone())?,
                DelegationSourceAuthorityId::new(binding.source_authority_id.clone())?,
                DelegationSourceRevision::new(binding.source_revision)?,
                DelegatedAuthorityId::new(binding.delegated_authority_id.clone())?,
            ))
        })
        .collect::<Result<Vec<_>, ExecutionEnvironmentError>>()?;
    let mut config =
        ExecutionEnvironmentConfig::new(limits, environment_delegations, credential_delegations)?;
    config.discovery_ceiling = wire.discovery_ceiling;
    process_options::validate_prefix(wire.required_path_prefix.as_deref())?;
    config
        .required_path_prefix
        .clone_from(&wire.required_path_prefix);
    Ok(config)
}
