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
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use plakous_model::{
    AttemptId, Clock, Digest, DigestAlgorithm, DigestDomain, IdGenerator, RunId, SafeDiagnostic,
    SystemClock, SystemIdGenerator,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{IsolationLevel, WorkspacePolicy};

mod backend;
mod process_options;
pub use process_options::HostProcessOptions;

pub use backend::*;

/// Digest format for reconstruction-complete execution-environment descriptors.
pub const EXECUTION_ENVIRONMENT_DIGEST_VERSION: u32 = 3;

/// Maximum bytes accepted in a portable environment or authority identifier.
pub const MAX_ENVIRONMENT_NAME_BYTES: usize = 128;

/// Maximum entries accepted in either explicit delegation list.
pub const MAX_ENVIRONMENT_ALLOWLIST_ENTRIES: usize = 256;

/// Maximum configured bytes for any individual or aggregate output limit.
pub const MAX_ENVIRONMENT_OUTPUT_BYTES: u64 = 1_073_741_824;

/// Maximum configured execution duration.
pub const MAX_ENVIRONMENT_EXECUTION_DURATION: Duration = Duration::from_secs(86_400);

/// Maximum configured cleanup grace duration.
pub const MAX_ENVIRONMENT_CLEANUP_DURATION: Duration = Duration::from_secs(3_600);

// 13: reported process snapshots retain bounded terminal tombstones.
const HOST_BACKEND_SEMANTIC_REVISION: u64 = 13;
const HOST_BACKEND_AUTHORITY_REVISION: u64 = 1;
const DETERMINISTIC_BACKEND_SEMANTIC_REVISION: u64 = 4;
const DETERMINISTIC_BACKEND_AUTHORITY_REVISION: u64 = 1;
const HOST_SNAPSHOT_SOURCE_AUTHORITY: &str = "source.environment.host-snapshot";
const HOST_SNAPSHOT_SOURCE_REVISION: u64 = 1;
const DEFAULT_STDOUT_BYTES: u64 = 1_048_576;
const DEFAULT_STDERR_BYTES: u64 = 262_144;
const DEFAULT_COMBINED_OUTPUT_BYTES: u64 = DEFAULT_STDOUT_BYTES + DEFAULT_STDERR_BYTES;
const DEFAULT_ARTIFACT_BYTES: u64 = 16_777_216;
const DEFAULT_EXECUTION_DURATION: Duration = Duration::from_secs(300);
// A joined process session outlives one foreground tool call: a polled build or
// test run keeps going between model rounds. Its own ceiling is the session
// duration; the foreground tool's `timeout` ends a joined call earlier.
const DEFAULT_SESSION_DURATION: Duration = Duration::from_secs(36_000);
const DEFAULT_CLEANUP_DURATION: Duration = Duration::from_secs(10);

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

/// Typed execution backend implementation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ExecutionBackendKind {
    /// Execution would occur with the embedding host process's OS authority.
    Host,
    /// Deterministic test execution with no real child process.
    DeterministicFake,
}

/// Backend-owned semantic revision for authority and containment behavior.
///
/// This revision is minted by the backend implementation, not supplied to the
/// public host constructor. Deserialized revisions are comparison data only.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExecutionBackendSemanticRevision(NonZeroU64);

impl ExecutionBackendSemanticRevision {
    const fn host() -> Self {
        match NonZeroU64::new(HOST_BACKEND_SEMANTIC_REVISION) {
            Some(revision) => Self(revision),
            None => unreachable!(),
        }
    }

    const fn deterministic_fake() -> Self {
        match NonZeroU64::new(DETERMINISTIC_BACKEND_SEMANTIC_REVISION) {
            Some(revision) => Self(revision),
            None => unreachable!(),
        }
    }

    /// Returns the backend semantic revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Opaque identity of one concrete live backend-authority allocation.
///
/// Each host or fake backend allocation mints a new value. Deserialized values
/// are comparison data only and cannot instantiate a live backend.
#[derive(Clone, Copy, Eq, Hash, PartialEq, Serialize)]
pub struct ExecutionBackendAuthorityId([u8; 16]);

impl ExecutionBackendAuthorityId {
    fn new(bytes: [u8; 16]) -> Result<Self, ExecutionEnvironmentError> {
        if bytes == [0; 16] {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidIdentity,
                "execution backend authority ID is invalid",
            ));
        }
        Ok(Self(bytes))
    }

    const fn bytes(self) -> [u8; 16] {
        self.0
    }
}

impl<'de> Deserialize<'de> for ExecutionBackendAuthorityId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(<[u8; 16]>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for ExecutionBackendAuthorityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExecutionBackendAuthorityId(<redacted>)")
    }
}

/// Backend-owned revision of the concrete host-authority instance.
#[derive(Clone, Copy, Eq, Hash, PartialEq, Serialize)]
pub struct ExecutionBackendAuthorityRevision(NonZeroU64);

impl ExecutionBackendAuthorityRevision {
    const fn host() -> Self {
        match NonZeroU64::new(HOST_BACKEND_AUTHORITY_REVISION) {
            Some(revision) => Self(revision),
            None => unreachable!(),
        }
    }

    const fn deterministic_fake() -> Self {
        match NonZeroU64::new(DETERMINISTIC_BACKEND_AUTHORITY_REVISION) {
            Some(revision) => Self(revision),
            None => unreachable!(),
        }
    }

    /// Returns the concrete-authority revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl<'de> Deserialize<'de> for ExecutionBackendAuthorityRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        NonZeroU64::new(u64::deserialize(deserializer)?)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("backend authority revision must be nonzero"))
    }
}

impl fmt::Debug for ExecutionBackendAuthorityRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ExecutionBackendAuthorityRevision")
            .field(&self.get())
            .finish()
    }
}

impl fmt::Debug for ExecutionBackendSemanticRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ExecutionBackendSemanticRevision")
            .field(&self.get())
            .finish()
    }
}

impl Serialize for ExecutionBackendSemanticRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.get())
    }
}

impl<'de> Deserialize<'de> for ExecutionBackendSemanticRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        NonZeroU64::new(u64::deserialize(deserializer)?)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("backend semantic revision must be nonzero"))
    }
}

/// Host path bytes and separator semantics used by an identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum HostPathFormat {
    /// UTF-8 path identity on a Unix-family host.
    UnixUtf8,
    /// UTF-8 path identity on a Windows-family host.
    WindowsUtf8,
    /// UTF-8 path identity on another target family.
    OtherUtf8,
}

impl HostPathFormat {
    const fn current() -> Self {
        #[cfg(unix)]
        {
            Self::UnixUtf8
        }
        #[cfg(windows)]
        {
            Self::WindowsUtf8
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::OtherUtf8
        }
    }

    fn is_compatible_with(self, operating_system: &str) -> bool {
        match self {
            Self::WindowsUtf8 => operating_system == "windows",
            Self::UnixUtf8 => operating_system != "windows" && operating_system != "unknown",
            Self::OtherUtf8 => operating_system == "unknown",
        }
    }
}

/// Physical filesystem containment enforced by the backend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum FilesystemContainment {
    /// Host path-policy checks at validation time, without race-free containment.
    HostPathPolicyOnly,
    /// Linux descriptor-relative resolution beneath a verified root, refusing symlinks.
    LinuxDescriptorRelativeNoFollow,
}

/// Physical process containment enforced by the backend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ProcessContainment {
    /// A future process would inherit the embedding host process's authority.
    InheritsHostAuthority,
    /// Process behavior is simulated deterministically without spawning.
    DeterministicFake,
}

/// Physical network containment enforced by the backend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum NetworkContainment {
    /// Network access retains the embedding host process's ambient authority.
    InheritsHostAuthority,
    /// The deterministic backend performs no network operations.
    Denied,
}

/// Truthful physical containment descriptor for one backend.
///
/// This descriptor is not an effect grant. Network and destructive-action
/// authorization belongs separately to `ToolPolicy`, including when physical
/// host authority would permit an operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContainmentDescriptor {
    filesystem: FilesystemContainment,
    process: ProcessContainment,
    network: NetworkContainment,
}

impl ExecutionContainmentDescriptor {
    const fn linux_host() -> Self {
        Self {
            filesystem: FilesystemContainment::LinuxDescriptorRelativeNoFollow,
            process: ProcessContainment::InheritsHostAuthority,
            network: NetworkContainment::InheritsHostAuthority,
        }
    }

    fn current_host() -> Self {
        if cfg!(target_os = "linux") {
            Self::linux_host()
        } else {
            Self {
                filesystem: FilesystemContainment::HostPathPolicyOnly,
                process: ProcessContainment::InheritsHostAuthority,
                network: NetworkContainment::InheritsHostAuthority,
            }
        }
    }

    const fn deterministic_fake() -> Self {
        Self {
            filesystem: current_filesystem_containment(),
            process: ProcessContainment::DeterministicFake,
            network: NetworkContainment::Denied,
        }
    }

    /// Returns the physical filesystem containment level.
    #[must_use]
    pub const fn filesystem(self) -> FilesystemContainment {
        self.filesystem
    }

    /// Returns the physical process containment level.
    #[must_use]
    pub const fn process(self) -> ProcessContainment {
        self.process
    }

    /// Returns the physical network containment level.
    #[must_use]
    pub const fn network(self) -> NetworkContainment {
        self.network
    }
}

const fn current_filesystem_containment() -> FilesystemContainment {
    if cfg!(target_os = "linux") {
        FilesystemContainment::LinuxDescriptorRelativeNoFollow
    } else {
        FilesystemContainment::HostPathPolicyOnly
    }
}

/// Serializable backend semantics retained across an exact restart.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
pub struct ExecutionBackendDescriptor {
    kind: ExecutionBackendKind,
    semantic_revision: ExecutionBackendSemanticRevision,
    operating_system: String,
    path_format: HostPathFormat,
    containment: ExecutionContainmentDescriptor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionBackendDescriptorWire {
    kind: ExecutionBackendKind,
    semantic_revision: ExecutionBackendSemanticRevision,
    operating_system: String,
    path_format: HostPathFormat,
    containment: ExecutionContainmentDescriptor,
}

impl ExecutionBackendDescriptor {
    fn current_host() -> Self {
        let mut descriptor =
            Self::host_for_platform(std::env::consts::OS, HostPathFormat::current());
        descriptor.containment = ExecutionContainmentDescriptor::current_host();
        descriptor
    }

    fn deterministic_fake() -> Self {
        Self {
            kind: ExecutionBackendKind::DeterministicFake,
            semantic_revision: ExecutionBackendSemanticRevision::deterministic_fake(),
            operating_system: std::env::consts::OS.to_owned(),
            path_format: HostPathFormat::current(),
            containment: ExecutionContainmentDescriptor::deterministic_fake(),
        }
    }

    fn host_for_platform(operating_system: &str, path_format: HostPathFormat) -> Self {
        Self {
            kind: ExecutionBackendKind::Host,
            semantic_revision: ExecutionBackendSemanticRevision::host(),
            operating_system: operating_system.to_owned(),
            path_format,
            containment: if operating_system == "linux" {
                ExecutionContainmentDescriptor::linux_host()
            } else {
                ExecutionContainmentDescriptor::current_host()
            },
        }
    }

    /// Returns the backend implementation kind.
    #[must_use]
    pub const fn kind(&self) -> ExecutionBackendKind {
        self.kind
    }

    /// Returns the backend-owned semantic revision.
    #[must_use]
    pub const fn semantic_revision(&self) -> ExecutionBackendSemanticRevision {
        self.semantic_revision
    }

    /// Returns the non-secret compile-target OS tag.
    #[must_use]
    pub fn operating_system(&self) -> &str {
        &self.operating_system
    }

    /// Returns the host-local path identity format.
    #[must_use]
    pub const fn path_format(&self) -> HostPathFormat {
        self.path_format
    }

    /// Returns the physical containment descriptor.
    #[must_use]
    pub const fn containment(&self) -> ExecutionContainmentDescriptor {
        self.containment
    }
}

impl<'de> Deserialize<'de> for ExecutionBackendDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExecutionBackendDescriptorWire::deserialize(deserializer)?;
        let expected_revision = match wire.kind {
            ExecutionBackendKind::Host => ExecutionBackendSemanticRevision::host(),
            ExecutionBackendKind::DeterministicFake => {
                ExecutionBackendSemanticRevision::deterministic_fake()
            }
        };
        let expected_containment = match wire.kind {
            ExecutionBackendKind::Host => ExecutionContainmentDescriptor::current_host(),
            ExecutionBackendKind::DeterministicFake => {
                ExecutionContainmentDescriptor::deterministic_fake()
            }
        };
        if wire.semantic_revision != expected_revision
            || !is_platform_tag(&wire.operating_system)
            || !wire.path_format.is_compatible_with(&wire.operating_system)
            || wire.containment != expected_containment
        {
            return Err(serde::de::Error::custom(
                "execution backend descriptor has invalid semantics or containment",
            ));
        }
        Ok(Self {
            kind: wire.kind,
            semantic_revision: wire.semantic_revision,
            operating_system: wire.operating_system,
            path_format: wire.path_format,
            containment: wire.containment,
        })
    }
}

impl fmt::Debug for ExecutionBackendDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionBackendDescriptor")
            .field("kind", &self.kind)
            .field("semantic_revision", &self.semantic_revision)
            .field("operating_system", &self.operating_system)
            .field("path_format", &self.path_format)
            .field("containment", &self.containment)
            .finish()
    }
}

/// Fresh nonserializable identity of one live backend allocation.
#[derive(Clone, Eq, PartialEq)]
pub struct ExecutionBackendIdentity {
    descriptor: ExecutionBackendDescriptor,
    authority_id: ExecutionBackendAuthorityId,
    authority_revision: ExecutionBackendAuthorityRevision,
}

impl ExecutionBackendIdentity {
    fn current_host() -> Result<Self, ExecutionEnvironmentError> {
        Ok(Self {
            descriptor: ExecutionBackendDescriptor::current_host(),
            authority_id: fresh_backend_authority_id()?,
            authority_revision: ExecutionBackendAuthorityRevision::host(),
        })
    }

    pub(super) fn deterministic_fake() -> Result<Self, ExecutionEnvironmentError> {
        Ok(Self {
            descriptor: ExecutionBackendDescriptor::deterministic_fake(),
            authority_id: fresh_backend_authority_id()?,
            authority_revision: ExecutionBackendAuthorityRevision::deterministic_fake(),
        })
    }

    pub(crate) const fn descriptor(&self) -> &ExecutionBackendDescriptor {
        &self.descriptor
    }

    /// Returns the backend implementation kind.
    #[must_use]
    pub const fn kind(&self) -> ExecutionBackendKind {
        self.descriptor.kind
    }

    /// Returns the backend-owned semantic revision.
    #[must_use]
    pub const fn semantic_revision(&self) -> ExecutionBackendSemanticRevision {
        self.descriptor.semantic_revision
    }

    /// Returns the opaque backend-allocation authority identity.
    #[must_use]
    pub const fn authority_id(&self) -> ExecutionBackendAuthorityId {
        self.authority_id
    }

    /// Returns the backend-allocation authority revision.
    #[must_use]
    pub const fn authority_revision(&self) -> ExecutionBackendAuthorityRevision {
        self.authority_revision
    }

    /// Returns the non-secret compile-target OS tag.
    #[must_use]
    pub fn operating_system(&self) -> &str {
        &self.descriptor.operating_system
    }

    /// Returns the host-local path identity format.
    #[must_use]
    pub const fn path_format(&self) -> HostPathFormat {
        self.descriptor.path_format
    }

    /// Returns the physical containment descriptor.
    #[must_use]
    pub const fn containment(&self) -> ExecutionContainmentDescriptor {
        self.descriptor.containment
    }
}

impl fmt::Debug for ExecutionBackendIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionBackendIdentity")
            .field("descriptor", &self.descriptor)
            .field("authority_id", &self.authority_id)
            .field("authority_revision", &self.authority_revision)
            .finish()
    }
}

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
    name: EnvironmentVariableName,
    source_authority_id: DelegationSourceAuthorityId,
    source_revision: DelegationSourceRevision,
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
    slot: CredentialSlotId,
    source_authority_id: DelegationSourceAuthorityId,
    source_revision: DelegationSourceRevision,
    delegated_authority_id: DelegatedAuthorityId,
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

/// Hard output and deadline limits carried by an environment identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionEnvironmentLimits {
    max_stdout_bytes: u64,
    max_stderr_bytes: u64,
    max_combined_output_bytes: u64,
    max_artifact_bytes: u64,
    execution_duration: Duration,
    session_duration: Duration,
    foreground_seconds: u64,
    cleanup_duration: Duration,
}

impl ExecutionEnvironmentLimits {
    /// Creates validated output and deadline limits. The joined-session
    /// duration starts equal to `execution_duration`; see
    /// [`Self::with_session_duration`].
    ///
    /// Each limit must be positive. The aggregate output limit must be at least
    /// each stream limit and no greater than their sum. Durations must be finite
    /// positive values within the exported hard maxima.
    ///
    /// # Errors
    ///
    /// Returns an invalid-limit error when limits are contradictory or outside
    /// hard bounds.
    #[allow(
        clippy::too_many_arguments,
        reason = "the six independent authority limits are intentionally explicit"
    )]
    pub fn new(
        max_stdout_bytes: u64,
        max_stderr_bytes: u64,
        max_combined_output_bytes: u64,
        max_artifact_bytes: u64,
        execution_duration: Duration,
        cleanup_duration: Duration,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let output_values = [
            max_stdout_bytes,
            max_stderr_bytes,
            max_combined_output_bytes,
            max_artifact_bytes,
        ];
        let output_valid = output_values
            .iter()
            .all(|value| (1..=MAX_ENVIRONMENT_OUTPUT_BYTES).contains(value));
        let stream_sum = max_stdout_bytes.saturating_add(max_stderr_bytes);
        let aggregate_valid = max_combined_output_bytes >= max_stdout_bytes
            && max_combined_output_bytes >= max_stderr_bytes
            && max_combined_output_bytes <= stream_sum;
        let deadlines_valid =
            duration_in_range(execution_duration, MAX_ENVIRONMENT_EXECUTION_DURATION)
                && duration_in_range(cleanup_duration, MAX_ENVIRONMENT_CLEANUP_DURATION);
        if !output_valid || !aggregate_valid || !deadlines_valid {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidLimit,
                "execution environment limits are invalid",
            ));
        }
        Ok(Self {
            max_stdout_bytes,
            max_stderr_bytes,
            max_combined_output_bytes,
            max_artifact_bytes,
            execution_duration,
            session_duration: execution_duration,
            foreground_seconds: 30,
            cleanup_duration,
        })
    }

    /// Replaces the joined process-session duration: the ceiling for one
    /// owned process session, which a polled command keeps running across
    /// model rounds. Literal processes keep `execution_duration`.
    ///
    /// # Errors
    ///
    /// Rejects a duration shorter than `execution_duration` or above
    /// [`MAX_ENVIRONMENT_EXECUTION_DURATION`].
    pub fn with_session_duration(
        mut self,
        session_duration: Duration,
    ) -> Result<Self, ExecutionEnvironmentError> {
        if session_duration < self.execution_duration
            || !duration_in_range(session_duration, MAX_ENVIRONMENT_EXECUTION_DURATION)
        {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidLimit,
                "execution environment session duration is invalid",
            ));
        }
        self.session_duration = session_duration;
        Ok(self)
    }

    /// Binds the default process observation budget into environment identity.
    ///
    /// # Errors
    /// Rejects budgets outside 1..=600 seconds.
    pub fn with_foreground_seconds(
        mut self,
        seconds: u64,
    ) -> Result<Self, ExecutionEnvironmentError> {
        if !(1..=600).contains(&seconds) {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidLimit,
                "foreground process budget is invalid",
            ));
        }
        self.foreground_seconds = seconds;
        Ok(self)
    }
    /// Returns the construction-bound default process hint in seconds.
    #[must_use]
    pub const fn foreground_seconds(self) -> u64 {
        self.foreground_seconds
    }

    /// Returns the per-operation stdout byte cap.
    #[must_use]
    pub const fn max_stdout_bytes(self) -> u64 {
        self.max_stdout_bytes
    }

    /// Returns the per-operation stderr byte cap.
    #[must_use]
    pub const fn max_stderr_bytes(self) -> u64 {
        self.max_stderr_bytes
    }

    /// Returns the aggregate stdout-plus-stderr byte cap.
    #[must_use]
    pub const fn max_combined_output_bytes(self) -> u64 {
        self.max_combined_output_bytes
    }

    /// Returns the per-artifact byte cap.
    #[must_use]
    pub const fn max_artifact_bytes(self) -> u64 {
        self.max_artifact_bytes
    }

    /// Returns the maximum execution duration.
    #[must_use]
    pub const fn execution_duration(self) -> Duration {
        self.execution_duration
    }

    /// Returns the maximum lifetime of one joined process session.
    #[must_use]
    pub const fn session_duration(self) -> Duration {
        self.session_duration
    }

    /// Returns the cleanup grace duration.
    #[must_use]
    pub const fn cleanup_duration(self) -> Duration {
        self.cleanup_duration
    }
}

impl Default for ExecutionEnvironmentLimits {
    fn default() -> Self {
        Self {
            max_stdout_bytes: DEFAULT_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_STDERR_BYTES,
            max_combined_output_bytes: DEFAULT_COMBINED_OUTPUT_BYTES,
            max_artifact_bytes: DEFAULT_ARTIFACT_BYTES,
            execution_duration: DEFAULT_EXECUTION_DURATION,
            session_duration: DEFAULT_SESSION_DURATION,
            foreground_seconds: 30,
            cleanup_duration: DEFAULT_CLEANUP_DURATION,
        }
    }
}

/// Immutable deterministic environment authority configuration.
///
/// Delegation lists carry names plus exact non-secret resolver identities and
/// revisions. Values and credential material are deliberately absent, and both
/// lists default to empty.
#[derive(Clone, Eq, PartialEq)]
pub struct ExecutionEnvironmentConfig {
    limits: ExecutionEnvironmentLimits,
    environment_delegations: Arc<[EnvironmentDelegation]>,
    credential_delegations: Arc<[CredentialDelegation]>,
    required_path_prefix: Option<String>,
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

    /// Returns the exact delegation binding for `name`, if configured.
    #[must_use]
    pub fn environment_delegation(
        &self,
        name: &EnvironmentVariableName,
    ) -> Option<&EnvironmentDelegation> {
        self.environment_delegations
            .binary_search_by(|binding| binding.name.cmp(name))
            .ok()
            .map(|index| &self.environment_delegations[index])
    }

    /// Returns the exact delegation binding for `slot`, if configured.
    #[must_use]
    pub fn credential_delegation(&self, slot: &CredentialSlotId) -> Option<&CredentialDelegation> {
        self.credential_delegations
            .binary_search_by(|binding| binding.slot.cmp(slot))
            .ok()
            .map(|index| &self.credential_delegations[index])
    }
}

impl Default for ExecutionEnvironmentConfig {
    fn default() -> Self {
        Self {
            limits: ExecutionEnvironmentLimits::default(),
            environment_delegations: Arc::from([]),
            credential_delegations: Arc::from([]),
            required_path_prefix: None,
        }
    }
}

impl fmt::Debug for ExecutionEnvironmentConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionEnvironmentConfig")
            .field("limits", &self.limits)
            .field("required_path_prefix", &self.required_path_prefix.is_some())
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

/// Stable filesystem-object identity for a canonical workspace root.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostFilesystemObjectIdentity {
    device: u64,
    inode: u64,
}

impl HostFilesystemObjectIdentity {
    /// Returns the Unix device identity captured for the root.
    #[must_use]
    pub const fn device(self) -> u64 {
        self.device
    }

    /// Returns the Unix inode identity captured for the root.
    #[must_use]
    pub const fn inode(self) -> u64 {
        self.inode
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentDelegationWire {
    name: String,
    source_authority_id: String,
    source_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialDelegationWire {
    slot: String,
    source_authority_id: String,
    source_revision: u64,
    delegated_authority_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExecutionEnvironmentLimitsWire {
    max_stdout_bytes: u64,
    max_stderr_bytes: u64,
    max_combined_output_bytes: u64,
    max_artifact_bytes: u64,
    execution_seconds: u64,
    execution_nanoseconds: u32,
    session_seconds: u64,
    foreground_seconds: u64,
    session_nanoseconds: u32,
    cleanup_seconds: u64,
    cleanup_nanoseconds: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExecutionEnvironmentDescriptorWire {
    backend: ExecutionBackendDescriptor,
    scope: ExecutionEnvironmentScope,
    canonical_root: String,
    canonical_cwd: String,
    root_object: HostFilesystemObjectIdentity,
    limits: ExecutionEnvironmentLimitsWire,
    environment_delegations: Vec<EnvironmentDelegationWire>,
    credential_delegations: Vec<CredentialDelegationWire>,
    required_path_prefix: Option<String>,
    digest: Digest,
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
    backend: ExecutionBackendDescriptor,
    scope: ExecutionEnvironmentScope,
    canonical_root: Arc<str>,
    canonical_cwd: Arc<str>,
    root_object: HostFilesystemObjectIdentity,
    config: Arc<ExecutionEnvironmentConfig>,
    digest: Digest,
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
    descriptor: ExecutionEnvironmentDescriptor,
    live_backend: ExecutionBackendIdentity,
    workspace: Arc<WorkspacePolicy>,
    relative_cwd: Arc<PathBuf>,
    config: Arc<ExecutionEnvironmentConfig>,
    operation_backend: Arc<dyn ExecutionBackend>,
    joined_process_sessions: Arc<JoinedProcessServiceCache>,
    host_process: Option<Arc<HostProcessContext>>,
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
        let process = Arc::new(HostProcessContext::capture(&workspace, options)?);
        let backend: Arc<dyn ExecutionBackend> =
            Arc::new(HostExecutionBackend::new(&config, Arc::clone(&process))?);
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

    fn with_operation_backend(
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

    fn from_workspace_and_backend(
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
        })
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

    fn recompute_digest(&self) -> Result<Digest, ExecutionEnvironmentError> {
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

fn fresh_backend_authority_id() -> Result<ExecutionBackendAuthorityId, ExecutionEnvironmentError> {
    SystemIdGenerator
        .next_uuid(SystemClock.now())
        .map(|identity| *identity.as_bytes())
        .map_err(|_| {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidIdentity,
                "execution backend authority ID could not be generated",
            )
        })
        .and_then(ExecutionBackendAuthorityId::new)
}

fn has_supported_digest_format(digest: Digest) -> bool {
    digest.algorithm() == DigestAlgorithm::Sha256
        && digest.domain() == DigestDomain::ExecutionEnvironment
        && digest.version() == EXECUTION_ENVIRONMENT_DIGEST_VERSION
}

fn is_platform_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ENVIRONMENT_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_lowercase_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    valid_first
        && value.len() <= MAX_ENVIRONMENT_NAME_BYTES
        && bytes.all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn redacted_identifier_debug(
    name: &'static str,
    bytes: usize,
    formatter: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    formatter
        .debug_struct(name)
        .field("bytes", &bytes)
        .field("value", &"<redacted>")
        .finish()
}

fn duration_in_range(duration: Duration, maximum: Duration) -> bool {
    !duration.is_zero() && duration <= maximum
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

fn path_identity_bytes(path: &Path) -> Result<Vec<u8>, ExecutionEnvironmentError> {
    path.to_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or_else(|| {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::Workspace,
                "execution workspace path is not valid UTF-8",
            )
        })
}

#[cfg(unix)]
fn filesystem_object_identity(
    path: &Path,
) -> Result<HostFilesystemObjectIdentity, ExecutionEnvironmentError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).map_err(|_| {
        ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::Workspace,
            "execution workspace root object identity could not be obtained",
        )
    })?;
    Ok(HostFilesystemObjectIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn filesystem_object_identity(
    _path: &Path,
) -> Result<HostFilesystemObjectIdentity, ExecutionEnvironmentError> {
    Err(ExecutionEnvironmentError::new(
        ExecutionEnvironmentErrorKind::Workspace,
        "stable execution workspace root identity is unsupported on this host",
    ))
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
    process_options::validate_prefix(wire.required_path_prefix.as_deref())?;
    config
        .required_path_prefix
        .clone_from(&wire.required_path_prefix);
    Ok(config)
}

fn environment_digest(
    backend: &ExecutionBackendDescriptor,
    scope: ExecutionEnvironmentScope,
    canonical_root: &[u8],
    canonical_cwd: &[u8],
    root_object: HostFilesystemObjectIdentity,
    config: &ExecutionEnvironmentConfig,
) -> Result<Digest, ExecutionEnvironmentError> {
    let containment = backend.containment();
    let mut parts = vec![
        b"plakous-tools/execution-environment".to_vec(),
        backend_kind_identifier(backend.kind()).to_vec(),
        backend.semantic_revision().get().to_be_bytes().to_vec(),
        backend.operating_system().as_bytes().to_vec(),
        path_format_identifier(backend.path_format()).to_vec(),
        b"isolation/host".to_vec(),
        filesystem_containment_identifier(containment.filesystem()).to_vec(),
        process_containment_identifier(containment.process()).to_vec(),
        network_containment_identifier(containment.network()).to_vec(),
    ];
    push_scope_parts(&mut parts, scope);
    parts.extend([
        canonical_root.to_vec(),
        canonical_cwd.to_vec(),
        root_object.device.to_be_bytes().to_vec(),
        root_object.inode.to_be_bytes().to_vec(),
    ]);
    push_limit_parts(&mut parts, config.limits());
    parts.push(
        config
            .required_path_prefix
            .as_deref()
            .unwrap_or("")
            .as_bytes()
            .to_vec(),
    );
    parts.push(
        usize_to_u64(config.environment_delegations.len())
            .to_be_bytes()
            .to_vec(),
    );
    for binding in config.environment_delegations.iter() {
        parts.push(binding.name().as_str().as_bytes().to_vec());
        push_source_parts(
            &mut parts,
            binding.source_authority_id(),
            binding.source_revision(),
        );
    }
    parts.push(
        usize_to_u64(config.credential_delegations.len())
            .to_be_bytes()
            .to_vec(),
    );
    for binding in config.credential_delegations.iter() {
        parts.push(binding.slot().as_str().as_bytes().to_vec());
        push_source_parts(
            &mut parts,
            binding.source_authority_id(),
            binding.source_revision(),
        );
        parts.push(
            binding
                .delegated_authority_id()
                .as_str()
                .as_bytes()
                .to_vec(),
        );
    }
    let borrowed = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        EXECUTION_ENVIRONMENT_DIGEST_VERSION,
        &borrowed,
    )
    .map_err(|_| {
        ExecutionEnvironmentError::new(
            ExecutionEnvironmentErrorKind::InvalidIdentity,
            "execution environment digest could not be computed",
        )
    })
}

fn push_scope_parts(parts: &mut Vec<Vec<u8>>, scope: ExecutionEnvironmentScope) {
    match scope {
        ExecutionEnvironmentScope::Run(run_id) => {
            parts.push(b"scope/run".to_vec());
            parts.push(run_id.to_string().into_bytes());
        }
        ExecutionEnvironmentScope::Attempt { run_id, attempt_id } => {
            parts.push(b"scope/attempt".to_vec());
            parts.push(run_id.to_string().into_bytes());
            parts.push(attempt_id.to_string().into_bytes());
        }
    }
}

fn push_limit_parts(parts: &mut Vec<Vec<u8>>, limits: ExecutionEnvironmentLimits) {
    parts.push(limits.foreground_seconds().to_be_bytes().to_vec());
    parts.extend([
        limits.max_stdout_bytes().to_be_bytes().to_vec(),
        limits.max_stderr_bytes().to_be_bytes().to_vec(),
        limits.max_combined_output_bytes().to_be_bytes().to_vec(),
        limits.max_artifact_bytes().to_be_bytes().to_vec(),
        limits.execution_duration().as_secs().to_be_bytes().to_vec(),
        limits
            .execution_duration()
            .subsec_nanos()
            .to_be_bytes()
            .to_vec(),
        limits.cleanup_duration().as_secs().to_be_bytes().to_vec(),
        limits
            .cleanup_duration()
            .subsec_nanos()
            .to_be_bytes()
            .to_vec(),
        limits.session_duration().as_secs().to_be_bytes().to_vec(),
        limits
            .session_duration()
            .subsec_nanos()
            .to_be_bytes()
            .to_vec(),
    ]);
}

fn push_source_parts(
    parts: &mut Vec<Vec<u8>>,
    source_authority_id: &DelegationSourceAuthorityId,
    source_revision: DelegationSourceRevision,
) {
    parts.push(source_authority_id.as_str().as_bytes().to_vec());
    parts.push(source_revision.get().to_be_bytes().to_vec());
}

const fn backend_kind_identifier(kind: ExecutionBackendKind) -> &'static [u8] {
    match kind {
        ExecutionBackendKind::Host => b"backend/host",
        ExecutionBackendKind::DeterministicFake => b"backend/deterministic-fake",
    }
}

const fn path_format_identifier(format: HostPathFormat) -> &'static [u8] {
    match format {
        HostPathFormat::UnixUtf8 => b"path-format/unix-utf8",
        HostPathFormat::WindowsUtf8 => b"path-format/windows-utf8",
        HostPathFormat::OtherUtf8 => b"path-format/other-utf8",
    }
}

const fn filesystem_containment_identifier(containment: FilesystemContainment) -> &'static [u8] {
    match containment {
        FilesystemContainment::HostPathPolicyOnly => {
            b"filesystem-containment/host-path-policy-only"
        }
        FilesystemContainment::LinuxDescriptorRelativeNoFollow => {
            b"filesystem-containment/linux-descriptor-relative-no-follow"
        }
    }
}

const fn process_containment_identifier(containment: ProcessContainment) -> &'static [u8] {
    match containment {
        ProcessContainment::InheritsHostAuthority => b"process-containment/inherits-host-authority",
        ProcessContainment::DeterministicFake => b"process-containment/deterministic-fake",
    }
}

const fn network_containment_identifier(containment: NetworkContainment) -> &'static [u8] {
    match containment {
        NetworkContainment::InheritsHostAuthority => b"network-containment/inherits-host-authority",
        NetworkContainment::Denied => b"network-containment/denied",
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

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
            "fb28f8b062472d20c0291ec41df011d07dbe9fd1bef713ae87b93d8f2df8c501"
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
