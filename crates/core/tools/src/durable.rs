//! Reconstruction-complete durable tool semantics and process-local bindings.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_model::{
    Digest, DigestDomain, ProjectedToolPresentationV2, SemanticJsonV1, ToolDefinition,
    ToolInputContract, ToolName,
};
use serde::{Deserialize, Serialize};

use crate::{
    ArgumentLimits, FrozenToolRegistry, NestedToolEligibility, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, RegisteredToolId, RegisteredToolIdentity, SchemaLimits,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolExposure, ToolExposureMap,
    ToolOwnerClass, ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMap,
    ToolProjectionRaisingV2, ToolRegistration, ToolRegistryBuilder, ToolRegistryLimits,
    ToolRuntimeError, ValidatedToolSchema,
};

/// Pre-integration V1 registry revision used only by the retained custom-profile path.
pub const TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION: u32 = 2;
/// Pre-integration V1 exposure revision used only by the retained custom-profile path.
pub const TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION: u32 = 1;
/// Combined-target V2 reconstruction-complete registry revision.
pub const TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION: u32 = 3;
/// Combined-target V2 reconstruction-complete projection revision.
pub const TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION: u32 = 3;

const REGISTRY_MAGIC: [u8; 8] = *b"PLKTRG02";
const EXPOSURE_MAGIC: [u8; 8] = *b"PLKTEX01";
const REGISTRY_V2_MAGIC: [u8; 8] = *b"PLKTRG03";
const EXPOSURE_V2_MAGIC: [u8; 8] = *b"PLKTEX03";
const HEADER_BYTES: usize = 12;
const MAX_DESCRIPTOR_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESERVED_PREFIXES: usize = 256;

/// Fail-closed durable descriptor decoding or integrity failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolDescriptorError {
    /// The byte envelope is empty or exceeds its fixed bound.
    Size,
    /// The byte envelope belongs to another component.
    InvalidMagic,
    /// No decoder is enrolled for this exact revision.
    UnsupportedVersion {
        /// Exact unsupported revision found in the envelope.
        found: u32,
    },
    /// Required data is missing, unknown, malformed, or has trailing bytes.
    Malformed,
    /// The byte representation or ordered collections are not canonical.
    NonCanonical,
    /// A retained digest disagrees with reconstructed semantic data.
    IntegrityMismatch,
    /// Registry limits, identities, definitions, or exposure mapping are invalid.
    InvalidSemantics,
}

impl fmt::Display for ToolDescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Size => "tool descriptor exceeds its size contract",
            Self::InvalidMagic => "tool descriptor has invalid magic",
            Self::UnsupportedVersion { .. } => "tool descriptor revision is unsupported",
            Self::Malformed => "tool descriptor is malformed",
            Self::NonCanonical => "tool descriptor encoding is noncanonical",
            Self::IntegrityMismatch => "tool descriptor integrity check failed",
            Self::InvalidSemantics => "tool descriptor semantics are invalid",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ToolDescriptorError {}

/// Exact process-local executor resolution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolBindingError {
    /// One stable ID was bound more than once while freezing the host table.
    DuplicateBinding {
        /// Exact duplicated stable executor ID.
        id: ToolExecutorId,
    },
    /// A persisted registration has no exact host binding.
    MissingBinding {
        /// Exact persisted stable executor ID unavailable from the host.
        id: ToolExecutorId,
    },
    /// A stable ID resolved to a different executable configuration.
    BindingIdentityMismatch {
        /// Exact stable executor ID whose binding drifted.
        id: ToolExecutorId,
        /// Durable binding identity required by the registration.
        expected: ToolExecutorBindingIdentity,
        /// Process-local binding identity supplied by the host.
        found: ToolExecutorBindingIdentity,
    },
    /// Persisted semantics failed reconstruction or integrity validation.
    Descriptor(ToolDescriptorError),
}

impl fmt::Display for ToolBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBinding { id } => {
                write!(formatter, "duplicate tool executor binding: {id}")
            }
            Self::MissingBinding { id } => write!(formatter, "missing tool executor binding: {id}"),
            Self::BindingIdentityMismatch { id, .. } => {
                write!(formatter, "tool executor binding identity mismatch: {id}")
            }
            Self::Descriptor(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ToolBindingError {}

impl From<ToolDescriptorError> for ToolBindingError {
    fn from(value: ToolDescriptorError) -> Self {
        Self::Descriptor(value)
    }
}

/// Immutable process-local executor capabilities keyed by durable IDs.
///
/// The table also carries which registered tools its host flagged
/// [`ToolRegistration::presented_name_claimer`] and every non-local
/// [`ToolRegistration::with_owner`]: both are host authority, never
/// persisted, so reconstruction re-takes them here.
#[derive(Clone)]
pub struct ToolExecutorBindings {
    by_id: BTreeMap<ToolExecutorId, BoundToolExecutor>,
    claimers: BTreeSet<RegisteredToolId>,
    owners: BTreeMap<RegisteredToolId, ToolOwnerClass>,
}

#[derive(Clone)]
struct BoundToolExecutor {
    identity: ToolExecutorBindingIdentity,
    executor: Arc<dyn ToolExecutor>,
}

impl fmt::Debug for ToolExecutorBindings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolExecutorBindings")
            .field("binding_count", &self.by_id.len())
            .field("claimers", &self.claimers)
            .field("owners", &self.owners)
            .finish()
    }
}

impl ToolExecutorBindings {
    /// Captures the process-local executable capabilities from one frozen registry.
    ///
    /// The resulting table carries no tool definitions or durable semantic
    /// authority; it keeps the registry's presented-name claimer flags.
    #[must_use]
    pub fn capture(registry: &FrozenToolRegistry) -> Self {
        let mut by_id = BTreeMap::new();
        for (id, identity, executor) in registry.durable_executor_bindings() {
            by_id
                .entry(id.clone())
                .or_insert_with(|| BoundToolExecutor {
                    identity,
                    executor: Arc::clone(executor),
                });
        }
        let claimers = registry
            .identities()
            .filter(|identity| identity.claims_presented_name())
            .map(|identity| identity.tool_id().clone())
            .collect();
        let owners = registry
            .identities()
            .filter(|identity| *identity.owner() != ToolOwnerClass::PlakLocal)
            .map(|identity| (identity.tool_id().clone(), identity.owner().clone()))
            .collect();
        Self {
            by_id,
            claimers,
            owners,
        }
    }

    /// Returns the number of process-local bindings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Reports whether the table has no bindings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// Single-use builder that rejects ambiguous host bindings.
#[derive(Default)]
pub struct ToolExecutorBindingsBuilder {
    by_id: BTreeMap<ToolExecutorId, BoundToolExecutor>,
    claimers: BTreeSet<RegisteredToolId>,
    owners: BTreeMap<RegisteredToolId, ToolOwnerClass>,
}

impl ToolExecutorBindingsBuilder {
    /// Creates an empty process-local binding builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            by_id: BTreeMap::new(),
            claimers: BTreeSet::new(),
            owners: BTreeMap::new(),
        }
    }

    /// Flags the registered tool `tool_id` as a presented-name claimer on
    /// reconstruction, as [`ToolRegistration::presented_name_claimer`] does at
    /// registration.
    pub fn presented_name_claimer(&mut self, tool_id: RegisteredToolId) -> &mut Self {
        self.claimers.insert(tool_id);
        self
    }

    /// Sets the owner of the registered tool `tool_id` on reconstruction, as
    /// [`ToolRegistration::with_owner`] does at registration.
    pub fn owner(&mut self, tool_id: RegisteredToolId, owner: ToolOwnerClass) -> &mut Self {
        self.owners.insert(tool_id, owner);
        self
    }

    /// Adds one exact stable ID to native executor binding.
    ///
    /// # Errors
    ///
    /// Every duplicate ID is rejected, including duplicates of the same `Arc`.
    pub fn bind(
        &mut self,
        id: ToolExecutorId,
        identity: ToolExecutorBindingIdentity,
        executor: Arc<dyn ToolExecutor>,
    ) -> Result<&mut Self, ToolBindingError> {
        if self.by_id.contains_key(&id) {
            return Err(ToolBindingError::DuplicateBinding { id });
        }
        self.by_id
            .insert(id, BoundToolExecutor { identity, executor });
        Ok(self)
    }

    /// Freezes the table so resolution observes one immutable authority set.
    #[must_use]
    pub fn freeze(self) -> ToolExecutorBindings {
        ToolExecutorBindings {
            by_id: self.by_id,
            claimers: self.claimers,
            owners: self.owners,
        }
    }
}

fn exact_composition_marker_registrations() -> Result<Vec<RegistrationWireV1>, ToolDescriptorError>
{
    let mut registrations = Vec::new();
    // The durable identity of the three markers lives in `composition_markers`
    // beside the rest of the descriptor vocabulary, so this authority never
    // reaches the crates that compile the presentations they belong to.
    for register in [
        crate::composition_markers::register_codex_code_mode_markers_into,
        crate::composition_markers::register_product_javascript_markers_into,
        crate::composition_markers::register_python_ptc_markers_into,
    ] {
        let mut registry = ToolRegistryBuilder::default();
        register(&mut registry).map_err(invalid_runtime)?;
        registrations.extend(
            FrozenToolRegistryDescriptor::capture(&registry.freeze())?
                .wire
                .registrations,
        );
    }
    Ok(registrations)
}

fn canonical_executor_authority(
    registration: &RegistrationWireV1,
) -> Result<(String, Digest), ToolDescriptorError> {
    const PRODUCT_PLAN_PREFIX: &str = "plakous.standard.update-plan.v1.plak.session.";
    const PRODUCT_PLAN_SUFFIX: &str = ".r1";
    const NORMALIZED_PRODUCT_PLAN_ID: &str = "plakous.standard.update-plan.v1.product-session.r1";

    let Some(branch_identity) = registration.executor_id.strip_prefix(PRODUCT_PLAN_PREFIX) else {
        return Ok((
            registration.executor_id.clone(),
            registration.executor_binding_identity,
        ));
    };
    let Some(session_id) = branch_identity.strip_suffix(PRODUCT_PLAN_SUFFIX) else {
        return Err(ToolDescriptorError::InvalidSemantics);
    };
    let valid_session_id = session_id.len() == 36
        && session_id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        });
    if !valid_session_id {
        return Err(ToolDescriptorError::InvalidSemantics);
    }
    let authority =
        crate::standard_identity::PlanStateIdentity::new(format!("plak.session.{session_id}"), 1)
            .map_err(invalid_runtime)?;
    let executor_id = authority.executor_id().map_err(invalid_runtime)?;
    let mut expected_registry = ToolRegistryBuilder::default();
    expected_registry
        .register(
            crate::standard_identity::expected_update_plan_registration(&executor_id)
                .map_err(invalid_runtime)?,
        )
        .map_err(invalid_runtime)?;
    let expected = FrozenToolRegistryDescriptor::capture(&expected_registry.freeze())?
        .wire
        .registrations
        .into_iter()
        .find(|candidate| candidate.executor_id == registration.executor_id)
        .ok_or(ToolDescriptorError::InvalidSemantics)?;
    if registration != &expected {
        return Err(ToolDescriptorError::InvalidSemantics);
    }
    let normalized_binding = ToolExecutorBindingIdentity::new(
        4,
        &[
            b"standard-coding-tool",
            NORMALIZED_PRODUCT_PLAN_ID.as_bytes(),
        ],
    )
    .map_err(invalid_runtime)?
    .digest();
    Ok((NORMALIZED_PRODUCT_PLAN_ID.to_owned(), normalized_binding))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryWireV1 {
    limits: RegistryLimitsWireV1,
    registrations: Vec<RegistrationWireV1>,
    registry_identity: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryLimitsWireV1 {
    max_tools: u64,
    max_total_schema_bytes: u64,
    schema_max_bytes: u64,
    argument_max_bytes: u64,
    argument_max_depth: u64,
    argument_max_nodes: u64,
    argument_max_string_bytes: u64,
    reserved_prefixes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationWireV1 {
    registered_tool_id: String,
    executor_id: String,
    executor_binding_identity: Digest,
    definition: ToolDefinition,
    nested_eligibility: NestedToolEligibility,
    identity: IdentityWireV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityWireV1 {
    registered_tool_id: String,
    name: ToolName,
    semantic_version: String,
    definition_digest: Digest,
    schema_digest: Digest,
    argument_limits_digest: Digest,
    executor_id: String,
    executor_binding_identity: Digest,
    effect_metadata_digest: Digest,
    nested_eligibility: NestedToolEligibility,
}

/// Complete durable registry semantics without native executable capabilities.
#[derive(Clone, Debug, PartialEq)]
pub struct FrozenToolRegistryDescriptor {
    wire: RegistryWireV1,
}

impl FrozenToolRegistryDescriptor {
    /// Captures complete, canonically ordered semantics from a frozen registry.
    ///
    /// # Errors
    ///
    /// Returns an integrity failure if the source snapshot cannot be revalidated.
    pub fn capture(registry: &FrozenToolRegistry) -> Result<Self, ToolDescriptorError> {
        let mut registrations = registry
            .durable_registrations()
            .map(|(identity, definition)| registration_wire(identity, definition.clone()))
            .collect::<Vec<_>>();
        registrations.sort_by(|left, right| left.registered_tool_id.cmp(&right.registered_tool_id));
        let mut limits = limits_wire(registry.limits())?;
        limits.reserved_prefixes.sort();
        limits.reserved_prefixes.dedup();
        let descriptor = Self {
            wire: RegistryWireV1 {
                limits,
                registrations,
                registry_identity: registry
                    .registry_identity_digest()
                    .map_err(invalid_runtime)?,
            },
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Encodes the sole canonical bounded V1 representation.
    ///
    /// # Errors
    ///
    /// Returns an error if the descriptor is invalid or exceeds its bound.
    pub fn encode_v1(&self) -> Result<Vec<u8>, ToolDescriptorError> {
        self.validate()?;
        encode_envelope(
            REGISTRY_MAGIC,
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            &self.wire,
        )
    }

    /// Decodes only the exact V1 representation; no legacy path exists.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, noncanonical, unsupported, or invalid bytes.
    pub fn decode_v1(bytes: &[u8]) -> Result<Self, ToolDescriptorError> {
        let wire = decode_envelope(
            bytes,
            REGISTRY_MAGIC,
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
        )?;
        let value = Self { wire };
        value.validate()?;
        Ok(value)
    }

    /// Encodes the exact target V2 registry representation.
    ///
    /// # Errors
    ///
    /// Rejects invalid semantics or an oversized encoding.
    pub fn encode_v2(&self) -> Result<Vec<u8>, ToolDescriptorError> {
        self.validate()?;
        encode_envelope(
            REGISTRY_V2_MAGIC,
            TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
            &self.wire,
        )
    }

    /// Decodes only the exact target V2 registry representation.
    ///
    /// # Errors
    ///
    /// Rejects malformed, noncanonical, predecessor, unknown, or invalid bytes.
    pub fn decode_v2(bytes: &[u8]) -> Result<Self, ToolDescriptorError> {
        let wire = decode_envelope(
            bytes,
            REGISTRY_V2_MAGIC,
            TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
        )?;
        let value = Self { wire };
        value.validate()?;
        Ok(value)
    }

    /// Returns the exact complete-registry identity witness.
    #[must_use]
    pub const fn registry_identity(&self) -> Digest {
        self.wire.registry_identity
    }

    /// Returns the persisted executor identity for one canonical tool name.
    ///
    /// This inspects registry bytes only. It never constructs or invokes an
    /// executor.
    #[must_use]
    pub fn executor_id_for_name(&self, name: &ToolName) -> Option<ToolExecutorId> {
        self.wire.registrations.iter().find_map(|registration| {
            (registration.identity.name == *name)
                .then(|| ToolExecutorId::new(registration.executor_id.clone()).ok())
                .flatten()
        })
    }

    /// Returns the canonical tool-interface identity without process-local
    /// executor bindings. This is the exact continuity witness used when a
    /// canonical rebind reconstructs the same tools for a new product branch.
    ///
    /// # Errors
    ///
    /// Returns an error if the descriptor is invalid or cannot be bounded.
    pub fn interface_identity(&self) -> Result<Digest, ToolDescriptorError> {
        self.validate()?;
        let limits = serde_json::to_vec(&self.wire.limits)
            .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
        let mut parts = vec![b"plakous-tools/registry-interface-v2".to_vec(), limits];
        for registration in &self.wire.registrations {
            let identity = &registration.identity;
            parts.push(registration.registered_tool_id.as_bytes().to_vec());
            parts.push(identity.name.as_str().as_bytes().to_vec());
            parts.push(identity.semantic_version.as_bytes().to_vec());
            parts.push(identity.definition_digest.identity_bytes());
            parts.push(identity.schema_digest.identity_bytes());
            parts.push(identity.argument_limits_digest.identity_bytes());
            parts.push(identity.effect_metadata_digest.identity_bytes());
            parts.push(identity.nested_eligibility.identity_bytes().to_vec());
        }
        let borrowed = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
        Digest::sha256(DigestDomain::ToolDefinition, 1, &borrowed)
            .map_err(|_| ToolDescriptorError::InvalidSemantics)
    }

    /// Returns the exact canonical executor/capability authority while
    /// excluding the closed set of driver-mediated composition markers.
    ///
    /// This permits an explicitly authorized target presentation to add or
    /// remove its outer composition entrypoints without allowing canonical
    /// tools, executor bindings, effects, validation bounds, or nesting
    /// eligibility to drift.
    ///
    /// # Errors
    ///
    /// Returns an error if the descriptor is invalid or cannot be bounded.
    pub fn canonical_authority_identity(&self) -> Result<Digest, ToolDescriptorError> {
        self.validate()?;
        let exact_markers = exact_composition_marker_registrations()?;
        let limits = serde_json::to_vec(&self.wire.limits)
            .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
        let mut parts = vec![
            b"plakous-tools/canonical-registry-authority-v1".to_vec(),
            limits,
        ];
        let mut authorities = Vec::new();
        for registration in &self.wire.registrations {
            if matches!(
                registration.executor_id.as_str(),
                crate::composition_markers::CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1
                    | crate::composition_markers::PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1
                    | crate::composition_markers::PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1
            ) {
                if !exact_markers.contains(registration) {
                    return Err(ToolDescriptorError::InvalidSemantics);
                }
                continue;
            }
            let identity = &registration.identity;
            let (executor_id, executor_binding_identity) =
                canonical_executor_authority(registration)?;
            authorities.push(
                Digest::sha256(
                    DigestDomain::ToolDefinition,
                    1,
                    &[
                        b"plakous-tools/canonical-executor-authority-v1",
                        executor_id.as_bytes(),
                        &executor_binding_identity.identity_bytes(),
                        &identity.effect_metadata_digest.identity_bytes(),
                        identity.nested_eligibility.identity_bytes(),
                    ],
                )
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?,
            );
        }
        authorities.sort_by_key(|identity| *identity.bytes());
        authorities.dedup();
        parts.extend(
            authorities
                .into_iter()
                .map(|identity| identity.identity_bytes()),
        );
        let borrowed = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
        Digest::sha256(DigestDomain::ToolDefinition, 1, &borrowed)
            .map_err(|_| ToolDescriptorError::InvalidSemantics)
    }

    /// Resolves only persisted executor IDs against one immutable host table.
    ///
    /// Extra host bindings are never copied into or addressable through the result.
    /// No executor method is called during reconstruction. A registration is
    /// a presented-name claimer exactly when the host table flags its
    /// registered tool ID; persisted bytes never carry or confer the flag.
    ///
    /// # Errors
    ///
    /// Returns an error if semantics are invalid or an exact executor binding is absent.
    pub fn resolve(
        &self,
        bindings: &ToolExecutorBindings,
    ) -> Result<FrozenToolRegistry, ToolBindingError> {
        let limits = self.validate()?;
        let mut builder = ToolRegistryBuilder::new(limits);
        for registration in &self.wire.registrations {
            let tool_id = RegisteredToolId::new(registration.registered_tool_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
            let executor_id = ToolExecutorId::new(registration.executor_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
            let expected =
                ToolExecutorBindingIdentity::from_digest(registration.executor_binding_identity);
            let binding = bindings.by_id.get(&executor_id).ok_or_else(|| {
                ToolBindingError::MissingBinding {
                    id: executor_id.clone(),
                }
            })?;
            if binding.identity != expected {
                return Err(ToolBindingError::BindingIdentityMismatch {
                    id: executor_id,
                    expected,
                    found: binding.identity,
                });
            }
            let claims = bindings.claimers.contains(&tool_id);
            let owner = bindings.owners.get(&tool_id).cloned().unwrap_or_default();
            let restored = ToolRegistration::new(
                tool_id,
                executor_id,
                expected,
                registration.definition.clone(),
                Arc::clone(&binding.executor),
            )
            .with_nested_eligibility(registration.nested_eligibility)
            .with_owner(owner);
            builder
                .register(if claims {
                    restored.presented_name_claimer()
                } else {
                    restored
                })
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
        }
        let registry = builder.freeze();
        if registry
            .registry_identity_digest()
            .map_err(invalid_runtime)?
            != self.wire.registry_identity
        {
            return Err(ToolDescriptorError::IntegrityMismatch.into());
        }
        Ok(registry)
    }

    fn validate(&self) -> Result<ToolRegistryLimits, ToolDescriptorError> {
        let limits = limits_from_wire(&self.wire.limits)?;
        crate::schema::validate_argument_limits(limits.arguments).map_err(invalid_runtime)?;
        if self.wire.registrations.len() > limits.max_tools {
            return Err(ToolDescriptorError::InvalidSemantics);
        }
        ensure_strict_order(
            self.wire
                .registrations
                .iter()
                .map(|entry| entry.registered_tool_id.as_str()),
        )?;
        ensure_strict_order(
            self.wire
                .limits
                .reserved_prefixes
                .iter()
                .map(String::as_str),
        )?;
        if self.wire.limits.reserved_prefixes.len() > MAX_RESERVED_PREFIXES {
            return Err(ToolDescriptorError::InvalidSemantics);
        }
        let mut names = BTreeSet::new();
        let mut total_schema_bytes = 0_usize;
        let argument_digest =
            crate::registry::argument_limits_digest(limits.arguments).map_err(invalid_runtime)?;
        let provenance = Arc::new(crate::registry::RegistryProvenance);
        let mut identities = Vec::with_capacity(self.wire.registrations.len());
        for entry in &self.wire.registrations {
            let tool_id = RegisteredToolId::new(entry.registered_tool_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
            let executor_id = ToolExecutorId::new(entry.executor_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
            if !names.insert(entry.definition.name().clone())
                || limits
                    .reserved_prefixes
                    .iter()
                    .any(|prefix| entry.definition.name().as_str().starts_with(prefix))
            {
                return Err(ToolDescriptorError::InvalidSemantics);
            }
            let schema =
                ValidatedToolSchema::compile(&entry.definition, limits.schema, limits.arguments)
                    .map_err(invalid_runtime)?;
            if let ToolInputContract::Structured(contract) = entry.definition.input() {
                total_schema_bytes = total_schema_bytes
                    .checked_add(contract.document().canonical_bytes().len())
                    .ok_or(ToolDescriptorError::InvalidSemantics)?;
            }
            let identity = RegisteredToolIdentity {
                tool_id,
                name: entry.definition.name().clone(),
                semantic_version: Arc::from(entry.definition.semantic_version()),
                definition_digest: entry.definition.digest(),
                schema_digest: schema.schema_digest(),
                argument_limits_digest: argument_digest,
                executor_id,
                executor_binding_identity: ToolExecutorBindingIdentity::from_digest(
                    entry.executor_binding_identity,
                ),
                effect_metadata_digest: crate::registry::effect_metadata_digest(
                    entry.definition.effect(),
                )
                .map_err(invalid_runtime)?,
                nested_eligibility: entry.nested_eligibility,
                claims_presented_name: false,
                owner: ToolOwnerClass::PlakLocal,
                provenance: Arc::clone(&provenance),
            };
            if identity_wire(&identity) != entry.identity {
                return Err(ToolDescriptorError::IntegrityMismatch);
            }
            identities.push(identity);
        }
        if total_schema_bytes > limits.max_total_schema_bytes {
            return Err(ToolDescriptorError::InvalidSemantics);
        }
        let digest = crate::exposure::registry_identity_digest_from_identities(&identities)
            .map_err(invalid_runtime)?;
        if digest != self.wire.registry_identity {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        Ok(limits)
    }

    /// The identity of one persisted registration, for digest checks only:
    /// the claimer flag is in no digest, and only [`Self::resolve`] re-takes
    /// it from the host table.
    fn identity_by_tool_id(
        &self,
        tool_id: &str,
    ) -> Result<(RegisteredToolIdentity, &ToolDefinition), ToolDescriptorError> {
        let entry = self
            .wire
            .registrations
            .binary_search_by(|entry| entry.registered_tool_id.as_str().cmp(tool_id))
            .ok()
            .and_then(|index| self.wire.registrations.get(index))
            .ok_or(ToolDescriptorError::InvalidSemantics)?;
        let limits = limits_from_wire(&self.wire.limits)?;
        let schema =
            ValidatedToolSchema::compile(&entry.definition, limits.schema, limits.arguments)
                .map_err(invalid_runtime)?;
        let identity = RegisteredToolIdentity {
            tool_id: RegisteredToolId::new(entry.registered_tool_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?,
            name: entry.definition.name().clone(),
            semantic_version: Arc::from(entry.definition.semantic_version()),
            definition_digest: entry.definition.digest(),
            schema_digest: schema.schema_digest(),
            argument_limits_digest: crate::registry::argument_limits_digest(limits.arguments)
                .map_err(invalid_runtime)?,
            executor_id: ToolExecutorId::new(entry.executor_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?,
            executor_binding_identity: ToolExecutorBindingIdentity::from_digest(
                entry.executor_binding_identity,
            ),
            effect_metadata_digest: crate::registry::effect_metadata_digest(
                entry.definition.effect(),
            )
            .map_err(invalid_runtime)?,
            nested_eligibility: entry.nested_eligibility,
            claims_presented_name: false,
            owner: ToolOwnerClass::PlakLocal,
            provenance: Arc::new(crate::registry::RegistryProvenance),
        };
        Ok((identity, &entry.definition))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExposureWireV1 {
    entries: Vec<ExposureEntryWireV1>,
    registry_identity: Digest,
    exposure_identity: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExposureEntryWireV1 {
    registered_tool_id: String,
    exposed_name: ToolName,
    description_override: Option<String>,
}

/// Complete durable mapping from registered identities to provider exposures.
#[derive(Clone, Debug, PartialEq)]
pub struct FrozenToolExposureDescriptor {
    wire: ExposureWireV1,
}

impl FrozenToolExposureDescriptor {
    /// Captures a complete exposure mapping, including canonical registration IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if the mapping cannot be represented exactly and boundedly.
    pub fn capture(exposure: &ToolExposureMap) -> Result<Self, ToolDescriptorError> {
        let mut entries = exposure
            .entries()
            .iter()
            .map(|entry| ExposureEntryWireV1 {
                registered_tool_id: entry.canonical_identity().tool_id().as_str().to_owned(),
                exposed_name: entry.exposed_name().clone(),
                description_override: entry.description_override().map(str::to_owned),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.registered_tool_id.cmp(&right.registered_tool_id));
        let value = Self {
            wire: ExposureWireV1 {
                entries,
                registry_identity: exposure.registry_digest(),
                exposure_identity: exposure.digest(),
            },
        };
        let registry = FrozenToolRegistryDescriptor::capture(exposure.registry())?;
        value.validate(&registry)?;
        Ok(value)
    }

    /// Encodes the sole canonical bounded V1 representation.
    ///
    /// # Errors
    ///
    /// Returns an error if the descriptor exceeds its canonical encoding bound.
    pub fn encode_v1(&self) -> Result<Vec<u8>, ToolDescriptorError> {
        encode_envelope(
            EXPOSURE_MAGIC,
            TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
            &self.wire,
        )
    }

    /// Decodes and exactly revalidates the mapping against retained registry semantics.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, noncanonical, mismatched, or invalid bytes.
    pub fn decode_v1(
        bytes: &[u8],
        registry: &FrozenToolRegistryDescriptor,
    ) -> Result<Self, ToolDescriptorError> {
        let wire = decode_envelope(
            bytes,
            EXPOSURE_MAGIC,
            TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
        )?;
        let value = Self { wire };
        value.validate(registry)?;
        Ok(value)
    }

    /// Returns the exact exposure identity witness.
    #[must_use]
    pub const fn exposure_identity(&self) -> Digest {
        self.wire.exposure_identity
    }

    /// Reconstructs the historical exposure over an exactly resolved registry.
    ///
    /// # Errors
    ///
    /// Returns an error if registry identity or exposure semantics do not match.
    pub fn resolve(
        &self,
        registry: FrozenToolRegistry,
    ) -> Result<ToolExposureMap, ToolDescriptorError> {
        if registry
            .registry_identity_digest()
            .map_err(invalid_runtime)?
            != self.wire.registry_identity
        {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        let mut exposures = Vec::with_capacity(self.wire.entries.len());
        for entry in &self.wire.entries {
            let identity = registry
                .identities()
                .find(|identity| identity.tool_id().as_str() == entry.registered_tool_id)
                .cloned()
                .ok_or(ToolDescriptorError::InvalidSemantics)?;
            exposures.push(
                ToolExposure::new(identity, entry.exposed_name.as_str().to_owned())
                    .and_then(|exposure| {
                        exposure.with_description_override(entry.description_override.clone())
                    })
                    .map_err(invalid_runtime)?,
            );
        }
        let resolved = ToolExposureMap::new(registry, exposures).map_err(invalid_runtime)?;
        if resolved.digest() != self.wire.exposure_identity {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        Ok(resolved)
    }

    fn validate(&self, registry: &FrozenToolRegistryDescriptor) -> Result<(), ToolDescriptorError> {
        registry.validate()?;
        if self.wire.registry_identity != registry.registry_identity() {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        ensure_strict_order(
            self.wire
                .entries
                .iter()
                .map(|entry| entry.registered_tool_id.as_str()),
        )?;
        let mut exposed_names = BTreeSet::new();
        let mut exposures = Vec::with_capacity(self.wire.entries.len());
        let mut definitions = Vec::with_capacity(self.wire.entries.len());
        for entry in &self.wire.entries {
            if !exposed_names.insert(entry.exposed_name.clone())
                || entry
                    .description_override
                    .as_deref()
                    .is_some_and(str::is_empty)
            {
                return Err(ToolDescriptorError::InvalidSemantics);
            }
            let (identity, definition) = registry.identity_by_tool_id(&entry.registered_tool_id)?;
            let exposure = ToolExposure::new(identity, entry.exposed_name.as_str().to_owned())
                .and_then(|value| {
                    value.with_description_override(entry.description_override.clone())
                })
                .map_err(invalid_runtime)?;
            definitions.push(
                crate::exposure::exposed_definition(definition, &exposure)
                    .map_err(invalid_runtime)?,
            );
            exposures.push(exposure);
        }
        let digest = crate::exposure::exposure_map_digest(&exposures, &definitions)
            .map_err(invalid_runtime)?;
        if digest != self.wire.exposure_identity {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionExposureWireV2 {
    canonical_operations: Vec<ProjectionOperationWireV2>,
    exposed_name: ToolName,
    description: String,
    presentation: ProjectedToolPresentationV2,
    defaults: SemanticJsonV1,
    lowering: ToolProjectionLoweringV2,
    lowering_identity: Digest,
    output_schema: PortableToolSchemaV2,
    raising: ToolProjectionRaisingV2,
    raising_identity: Digest,
    projection_identity: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionOperationWireV2 {
    operation: crate::CanonicalProcessOperationV2,
    registered_tool_id: String,
    canonical_revision: String,
    executor_id: String,
    executor_binding_identity: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionMapWireV2 {
    registry_identity: Digest,
    target: ProviderModelProjectionIdentityV2,
    entries: Vec<ProjectionExposureWireV2>,
    exposure_identity: Digest,
}

/// Reconstruction-complete V2 exact model projection descriptor.
///
/// This closed descriptor is the combined-target exposure representation. It
/// retains every lowering/raising/default/schema/target/effect/executor binding
/// and permits several exact exposed names to select one canonical capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenToolProjectionDescriptorV2 {
    wire: ProjectionMapWireV2,
}

impl FrozenToolProjectionDescriptorV2 {
    /// Captures one complete V2 projection map without executable capabilities.
    ///
    /// # Errors
    ///
    /// Rejects a map that cannot be represented and revalidated exactly.
    pub fn capture(map: &ToolProjectionMap) -> Result<Self, ToolDescriptorError> {
        let entries = map
            .entries()
            .iter()
            .map(|entry| ProjectionExposureWireV2 {
                canonical_operations: entry
                    .canonical_operations()
                    .iter()
                    .map(|(operation, identity)| ProjectionOperationWireV2 {
                        operation: *operation,
                        registered_tool_id: identity.tool_id().as_str().to_owned(),
                        canonical_revision: identity.semantic_version().to_owned(),
                        executor_id: identity.executor_id().as_str().to_owned(),
                        executor_binding_identity: identity.executor_binding_identity().digest(),
                    })
                    .collect(),
                exposed_name: entry.exposed_name().clone(),
                description: entry.description().to_owned(),
                presentation: entry.presentation().clone(),
                defaults: entry.defaults().clone(),
                lowering: entry.lowering(),
                lowering_identity: entry.lowering_identity(),
                output_schema: entry.output_schema().clone(),
                raising: entry.raising(),
                raising_identity: entry.raising_identity(),
                projection_identity: entry.identity(),
            })
            .collect();
        let value = Self {
            wire: ProjectionMapWireV2 {
                registry_identity: map.registry_digest(),
                target: map.target().clone(),
                entries,
                exposure_identity: map.digest(),
            },
        };
        let registry = FrozenToolRegistryDescriptor::capture(map.registry())?;
        value.validate(&registry)?;
        Ok(value)
    }

    /// Encodes the sole canonical bounded V2 projection representation.
    ///
    /// # Errors
    ///
    /// Rejects an oversized or non-serializable descriptor.
    pub fn encode_v2(&self) -> Result<Vec<u8>, ToolDescriptorError> {
        encode_envelope(
            EXPOSURE_V2_MAGIC,
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
            &self.wire,
        )
    }

    /// Decodes and revalidates only the exact V2 representation.
    ///
    /// # Errors
    ///
    /// Rejects malformed, noncanonical, mismatched, predecessor, or unknown bytes.
    pub fn decode_v2(
        bytes: &[u8],
        registry: &FrozenToolRegistryDescriptor,
    ) -> Result<Self, ToolDescriptorError> {
        let wire = decode_envelope(
            bytes,
            EXPOSURE_V2_MAGIC,
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
        )?;
        let value = Self { wire };
        value.validate(registry)?;
        Ok(value)
    }

    /// Exact qualified target retained by this descriptor.
    #[must_use]
    pub const fn target(&self) -> &ProviderModelProjectionIdentityV2 {
        &self.wire.target
    }

    /// Exact complete projection-map identity witness.
    #[must_use]
    pub const fn exposure_identity(&self) -> Digest {
        self.wire.exposure_identity
    }

    /// Resolves the descriptor over one exact executable registry snapshot.
    ///
    /// # Errors
    ///
    /// Rejects registry drift, missing canonical registrations, or any identity mismatch.
    pub fn resolve(
        &self,
        registry: FrozenToolRegistry,
    ) -> Result<ToolProjectionMap, ToolDescriptorError> {
        if registry
            .registry_identity_digest()
            .map_err(invalid_runtime)?
            != self.wire.registry_identity
        {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        let mut entries = Vec::with_capacity(self.wire.entries.len());
        for wire in &self.wire.entries {
            let identities = wire
                .canonical_operations
                .iter()
                .map(|operation| {
                    registry
                        .identities()
                        .find(|identity| {
                            identity.tool_id().as_str() == operation.registered_tool_id
                        })
                        .cloned()
                        .map(|identity| (operation.operation, identity))
                        .ok_or(ToolDescriptorError::InvalidSemantics)
                })
                .collect::<Result<Vec<_>, _>>()?;
            entries.push(projection_from_wire(
                wire,
                identities,
                self.wire.target.clone(),
            )?);
        }
        let map = ToolProjectionMap::new(registry, self.wire.target.clone(), entries)
            .map_err(invalid_runtime)?;
        if map.digest() != self.wire.exposure_identity {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        Ok(map)
    }

    fn validate(&self, registry: &FrozenToolRegistryDescriptor) -> Result<(), ToolDescriptorError> {
        registry.validate()?;
        if registry.registry_identity() != self.wire.registry_identity {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        let target = ProviderModelProjectionIdentityV2::new(
            self.wire.target.integration_class(),
            self.wire.target.protocol(),
            self.wire.target.route(),
            self.wire.target.integration_profile(),
            self.wire.target.family(),
            self.wire.target.requested_model(),
            self.wire.target.effective_model(),
            self.wire.target.effective_provider_configuration(),
            self.wire.target.projection_revision(),
        )
        .map_err(invalid_runtime)?;
        if target != self.wire.target {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        // Entry order is the exact model-visible presentation order, not a
        // sorted set: canonicality here is one appearance per exposed name.
        // Map identity stays set-ordered (`projection_map_digest`).
        let mut exposed_names = BTreeSet::new();
        for entry in &self.wire.entries {
            if !exposed_names.insert(entry.exposed_name.as_str()) {
                return Err(ToolDescriptorError::InvalidSemantics);
            }
        }
        let mut projections = Vec::with_capacity(self.wire.entries.len());
        for wire in &self.wire.entries {
            let identities = wire
                .canonical_operations
                .iter()
                .map(|operation| {
                    registry
                        .identity_by_tool_id(&operation.registered_tool_id)
                        .map(|(identity, _)| (operation.operation, identity))
                })
                .collect::<Result<Vec<_>, _>>()?;
            projections.push(projection_from_wire(wire, identities, target.clone())?);
        }
        let digest = crate::exposure::projection_map_digest(
            self.wire.registry_identity,
            &target,
            &projections,
        )
        .map_err(invalid_runtime)?;
        if digest != self.wire.exposure_identity {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
        Ok(())
    }
}

fn projection_from_wire(
    wire: &ProjectionExposureWireV2,
    identities: Vec<(crate::CanonicalProcessOperationV2, RegisteredToolIdentity)>,
    target: ProviderModelProjectionIdentityV2,
) -> Result<ToolProjectionExposureV2, ToolDescriptorError> {
    for ((operation, identity), expected) in identities.iter().zip(&wire.canonical_operations) {
        if operation != &expected.operation
            || identity.semantic_version() != expected.canonical_revision
            || identity.executor_id().as_str() != expected.executor_id
            || identity.executor_binding_identity().digest() != expected.executor_binding_identity
        {
            return Err(ToolDescriptorError::IntegrityMismatch);
        }
    }
    let projection = match &wire.presentation {
        ProjectedToolPresentationV2::StructuredFunction { input_schema } => {
            ToolProjectionExposureV2::new(
                identities,
                wire.exposed_name.as_str(),
                wire.description.clone(),
                input_schema.clone(),
                wire.defaults.clone(),
                wire.lowering,
                wire.output_schema.clone(),
                wire.raising,
                target,
            )
        }
        ProjectedToolPresentationV2::CustomUtf8 { input, format } => {
            ToolProjectionExposureV2::new_custom(
                identities,
                wire.exposed_name.as_str(),
                wire.description.clone(),
                *input,
                format.clone(),
                wire.defaults.clone(),
                wire.lowering,
                wire.output_schema.clone(),
                wire.raising,
                target,
            )
        }
    }
    .map_err(invalid_runtime)?;
    if projection.lowering_identity() != wire.lowering_identity
        || projection.raising_identity() != wire.raising_identity
        || projection.identity() != wire.projection_identity
    {
        return Err(ToolDescriptorError::IntegrityMismatch);
    }
    Ok(projection)
}

fn registration_wire(
    identity: &RegisteredToolIdentity,
    definition: ToolDefinition,
) -> RegistrationWireV1 {
    RegistrationWireV1 {
        registered_tool_id: identity.tool_id().as_str().to_owned(),
        executor_id: identity.executor_id().as_str().to_owned(),
        executor_binding_identity: identity.executor_binding_identity().digest(),
        definition,
        nested_eligibility: identity.nested_eligibility(),
        identity: identity_wire(identity),
    }
}

fn identity_wire(identity: &RegisteredToolIdentity) -> IdentityWireV1 {
    IdentityWireV1 {
        registered_tool_id: identity.tool_id().as_str().to_owned(),
        name: identity.name().clone(),
        semantic_version: identity.semantic_version().to_owned(),
        definition_digest: identity.definition_digest(),
        schema_digest: identity.schema_digest(),
        argument_limits_digest: identity.argument_limits_digest(),
        executor_id: identity.executor_id().as_str().to_owned(),
        executor_binding_identity: identity.executor_binding_identity().digest(),
        effect_metadata_digest: identity.effect_metadata_digest(),
        nested_eligibility: identity.nested_eligibility(),
    }
}

fn limits_wire(limits: &ToolRegistryLimits) -> Result<RegistryLimitsWireV1, ToolDescriptorError> {
    Ok(RegistryLimitsWireV1 {
        max_tools: portable(limits.max_tools)?,
        max_total_schema_bytes: portable(limits.max_total_schema_bytes)?,
        schema_max_bytes: portable(limits.schema.max_bytes)?,
        argument_max_bytes: portable(limits.arguments.max_bytes)?,
        argument_max_depth: portable(limits.arguments.max_depth)?,
        argument_max_nodes: portable(limits.arguments.max_nodes)?,
        argument_max_string_bytes: portable(limits.arguments.max_string_bytes)?,
        reserved_prefixes: limits.reserved_prefixes.clone(),
    })
}

fn limits_from_wire(
    wire: &RegistryLimitsWireV1,
) -> Result<ToolRegistryLimits, ToolDescriptorError> {
    Ok(ToolRegistryLimits {
        max_tools: local(wire.max_tools)?,
        max_total_schema_bytes: local(wire.max_total_schema_bytes)?,
        schema: SchemaLimits {
            max_bytes: local(wire.schema_max_bytes)?,
        },
        arguments: ArgumentLimits {
            max_bytes: local(wire.argument_max_bytes)?,
            max_depth: local(wire.argument_max_depth)?,
            max_nodes: local(wire.argument_max_nodes)?,
            max_string_bytes: local(wire.argument_max_string_bytes)?,
        },
        reserved_prefixes: wire.reserved_prefixes.clone(),
    })
}

fn portable(value: usize) -> Result<u64, ToolDescriptorError> {
    u64::try_from(value).map_err(|_| ToolDescriptorError::InvalidSemantics)
}

fn local(value: u64) -> Result<usize, ToolDescriptorError> {
    usize::try_from(value).map_err(|_| ToolDescriptorError::InvalidSemantics)
}

fn ensure_strict_order<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), ToolDescriptorError> {
    let mut previous = None;
    for value in values {
        if previous.is_some_and(|previous| previous >= value) {
            return Err(ToolDescriptorError::NonCanonical);
        }
        previous = Some(value);
    }
    Ok(())
}

fn encode_envelope<T: Serialize>(
    magic: [u8; 8],
    revision: u32,
    value: &T,
) -> Result<Vec<u8>, ToolDescriptorError> {
    let payload = serde_json::to_vec(value).map_err(|_| ToolDescriptorError::Malformed)?;
    let total = payload
        .len()
        .checked_add(HEADER_BYTES)
        .ok_or(ToolDescriptorError::Size)?;
    if payload.is_empty() || total > MAX_DESCRIPTOR_BYTES {
        return Err(ToolDescriptorError::Size);
    }
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&magic);
    bytes.extend_from_slice(&revision.to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

fn decode_envelope<T>(bytes: &[u8], magic: [u8; 8], revision: u32) -> Result<T, ToolDescriptorError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    if bytes.len() <= HEADER_BYTES || bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(ToolDescriptorError::Size);
    }
    if bytes[..8] != magic {
        return Err(ToolDescriptorError::InvalidMagic);
    }
    let found = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| ToolDescriptorError::Malformed)?,
    );
    if found != revision {
        return Err(ToolDescriptorError::UnsupportedVersion { found });
    }
    let value: T = serde_json::from_slice(&bytes[HEADER_BYTES..])
        .map_err(|_| ToolDescriptorError::Malformed)?;
    if serde_json::to_vec(&value).map_err(|_| ToolDescriptorError::Malformed)?
        != bytes[HEADER_BYTES..]
    {
        return Err(ToolDescriptorError::NonCanonical);
    }
    Ok(value)
}

fn invalid_runtime(_: ToolRuntimeError) -> ToolDescriptorError {
    ToolDescriptorError::InvalidSemantics
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
