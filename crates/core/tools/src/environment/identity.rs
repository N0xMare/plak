//! Backend identity, containment, and the execution-environment digest.

use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;

use plakous_model::{
    Clock, Digest, DigestAlgorithm, DigestDomain, IdGenerator, SystemClock, SystemIdGenerator,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{
    DelegationSourceAuthorityId, DelegationSourceRevision, ExecutionEnvironmentConfig,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
    ExecutionEnvironmentScope, MAX_ENVIRONMENT_NAME_BYTES,
};

/// Digest format for reconstruction-complete execution-environment descriptors.
// 5: the backend names the runner it executes on.
pub const EXECUTION_ENVIRONMENT_DIGEST_VERSION: u32 = 5;

// 13: reported process snapshots retain bounded terminal tombstones.
// 14: surviving shell groups retain joined ownership after leader exit.
// 15: reconstructive repository-discovery ceiling on every process spawn.
const HOST_BACKEND_SEMANTIC_REVISION: u64 = 15;

const HOST_BACKEND_AUTHORITY_REVISION: u64 = 1;

const DETERMINISTIC_BACKEND_SEMANTIC_REVISION: u64 = 4;

const DETERMINISTIC_BACKEND_AUTHORITY_REVISION: u64 = 1;

pub(super) const HOST_SNAPSHOT_SOURCE_AUTHORITY: &str = "source.environment.host-snapshot";

pub(super) const HOST_SNAPSHOT_SOURCE_REVISION: u64 = 1;

/// Typed execution backend implementation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ExecutionBackendKind {
    /// Execution on one runner, named by the descriptor's [`RunnerId`], with
    /// that runner's OS authority. The in-process host backend is the local
    /// runner ([`RunnerId::local`]).
    Runner,
    /// Deterministic test execution with no real child process.
    DeterministicFake,
}

/// The runner an execution backend runs tools on. Part of a session's
/// binding: every rebind of a line keeps it.
///
/// [`RunnerId::local`] is the runner on the embedding host's own machine and
/// user, whether its calls are made in process or through a local runner
/// daemon. Other runners are named by the identity their own home records.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerId(Box<str>);

impl RunnerId {
    const LOCAL: &'static str = "local";

    /// The runner on the embedding host's own machine and user.
    #[must_use]
    pub fn local() -> Self {
        Self(Self::LOCAL.into())
    }

    /// Admits a lowercase runner identifier of at most 128 bytes.
    ///
    /// # Errors
    ///
    /// Refuses anything else.
    pub fn new(value: impl Into<String>) -> Result<Self, ExecutionEnvironmentError> {
        let value = value.into();
        if !is_lowercase_identifier(&value) {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidIdentity,
                "runner id must be a lowercase identifier of at most 128 bytes",
            ));
        }
        Ok(Self(value.into()))
    }

    /// The identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the local runner.
    #[must_use]
    pub fn is_local(&self) -> bool {
        &*self.0 == Self::LOCAL
    }
}

impl fmt::Debug for RunnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RunnerId").field(&&*self.0).finish()
    }
}

impl fmt::Display for RunnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for RunnerId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RunnerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Backend-owned semantic revision for authority and containment behavior.
///
/// This revision is minted by the backend implementation, not supplied to the
/// public host constructor. Deserialized revisions are comparison data only.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExecutionBackendSemanticRevision(NonZeroU64);

impl ExecutionBackendSemanticRevision {
    pub(super) const fn host() -> Self {
        match NonZeroU64::new(HOST_BACKEND_SEMANTIC_REVISION) {
            Some(revision) => Self(revision),
            None => unreachable!(),
        }
    }

    pub(super) const fn deterministic_fake() -> Self {
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
    pub(super) fn new(bytes: [u8; 16]) -> Result<Self, ExecutionEnvironmentError> {
        if bytes == [0; 16] {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidIdentity,
                "execution backend authority ID is invalid",
            ));
        }
        Ok(Self(bytes))
    }

    pub(super) const fn bytes(self) -> [u8; 16] {
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
    pub(super) const fn host() -> Self {
        match NonZeroU64::new(HOST_BACKEND_AUTHORITY_REVISION) {
            Some(revision) => Self(revision),
            None => unreachable!(),
        }
    }

    pub(super) const fn deterministic_fake() -> Self {
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
    pub(super) const fn current() -> Self {
        Self::UnixUtf8
    }

    pub(super) fn is_compatible_with(self, operating_system: &str) -> bool {
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
    pub(super) filesystem: FilesystemContainment,
    pub(super) process: ProcessContainment,
    pub(super) network: NetworkContainment,
}

impl ExecutionContainmentDescriptor {
    pub(super) const fn linux_host() -> Self {
        Self {
            filesystem: FilesystemContainment::LinuxDescriptorRelativeNoFollow,
            process: ProcessContainment::InheritsHostAuthority,
            network: NetworkContainment::InheritsHostAuthority,
        }
    }

    pub(super) fn current_host() -> Self {
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

    pub(super) const fn deterministic_fake() -> Self {
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
    pub(super) kind: ExecutionBackendKind,
    /// The runner of a [`ExecutionBackendKind::Runner`] backend; absent for
    /// the deterministic fake.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) runner_id: Option<RunnerId>,
    pub(super) semantic_revision: ExecutionBackendSemanticRevision,
    pub(super) operating_system: String,
    pub(super) path_format: HostPathFormat,
    pub(super) containment: ExecutionContainmentDescriptor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionBackendDescriptorWire {
    pub(super) kind: ExecutionBackendKind,
    #[serde(default)]
    pub(super) runner_id: Option<RunnerId>,
    pub(super) semantic_revision: ExecutionBackendSemanticRevision,
    pub(super) operating_system: String,
    pub(super) path_format: HostPathFormat,
    pub(super) containment: ExecutionContainmentDescriptor,
}

impl ExecutionBackendDescriptor {
    /// The descriptor every [`HostExecutionBackend`](crate::HostExecutionBackend)
    /// allocated in this process presents: the local runner, on this
    /// process's OS. It is comparison data, not authority.
    #[must_use]
    pub fn local_runner() -> Self {
        let mut descriptor =
            Self::local_runner_for_platform(std::env::consts::OS, HostPathFormat::current());
        descriptor.containment = ExecutionContainmentDescriptor::current_host();
        descriptor
    }

    pub(super) fn deterministic_fake() -> Self {
        Self {
            kind: ExecutionBackendKind::DeterministicFake,
            runner_id: None,
            semantic_revision: ExecutionBackendSemanticRevision::deterministic_fake(),
            operating_system: std::env::consts::OS.to_owned(),
            path_format: HostPathFormat::current(),
            containment: ExecutionContainmentDescriptor::deterministic_fake(),
        }
    }

    pub(super) fn local_runner_for_platform(
        operating_system: &str,
        path_format: HostPathFormat,
    ) -> Self {
        Self {
            kind: ExecutionBackendKind::Runner,
            runner_id: Some(RunnerId::local()),
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

    /// Returns the runner this backend runs tools on; `None` for the
    /// deterministic fake.
    #[must_use]
    pub const fn runner_id(&self) -> Option<&RunnerId> {
        self.runner_id.as_ref()
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
            ExecutionBackendKind::Runner => ExecutionBackendSemanticRevision::host(),
            ExecutionBackendKind::DeterministicFake => {
                ExecutionBackendSemanticRevision::deterministic_fake()
            }
        };
        let expected_containment = match wire.kind {
            ExecutionBackendKind::Runner => ExecutionContainmentDescriptor::current_host(),
            ExecutionBackendKind::DeterministicFake => {
                ExecutionContainmentDescriptor::deterministic_fake()
            }
        };
        let runner_named = match wire.kind {
            ExecutionBackendKind::Runner => wire.runner_id.is_some(),
            ExecutionBackendKind::DeterministicFake => wire.runner_id.is_none(),
        };
        if !runner_named
            || wire.semantic_revision != expected_revision
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
            runner_id: wire.runner_id,
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
            .field("runner_id", &self.runner_id)
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
    pub(super) descriptor: ExecutionBackendDescriptor,
    pub(super) authority_id: ExecutionBackendAuthorityId,
    pub(super) authority_revision: ExecutionBackendAuthorityRevision,
}

impl ExecutionBackendIdentity {
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "read only by the unix host backend")
    )]
    pub(super) fn local_runner() -> Result<Self, ExecutionEnvironmentError> {
        Ok(Self {
            descriptor: ExecutionBackendDescriptor::local_runner(),
            authority_id: fresh_backend_authority_id()?,
            authority_revision: ExecutionBackendAuthorityRevision::host(),
        })
    }

    /// A runner-backed authority presenting the runner's own `descriptor`.
    pub(super) fn runner(
        descriptor: ExecutionBackendDescriptor,
    ) -> Result<Self, ExecutionEnvironmentError> {
        Ok(Self {
            descriptor,
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

    /// Returns the runner this backend runs tools on.
    #[must_use]
    pub const fn runner_id(&self) -> Option<&RunnerId> {
        self.descriptor.runner_id()
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

/// Stable filesystem-object identity for a canonical workspace root.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostFilesystemObjectIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
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

pub(super) fn has_supported_digest_format(digest: Digest) -> bool {
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

pub(super) fn is_lowercase_identifier(value: &str) -> bool {
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

pub(super) fn redacted_identifier_debug(
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

pub(super) fn path_identity_bytes(path: &Path) -> Result<Vec<u8>, ExecutionEnvironmentError> {
    path.to_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or_else(|| {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::Workspace,
                "execution workspace path is not valid UTF-8",
            )
        })
}

pub(crate) fn filesystem_object_identity(
    path: &Path,
) -> Result<HostFilesystemObjectIdentity, ExecutionEnvironmentError> {
    use super::metadata::MetadataExt;

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

pub(super) fn environment_digest(
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
        backend
            .runner_id()
            .map_or(&b""[..], |runner| runner.as_str().as_bytes())
            .to_vec(),
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
    parts.push(vec![u8::from(config.discovery_ceiling)]);
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
        ExecutionBackendKind::Runner => b"backend/runner",
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
