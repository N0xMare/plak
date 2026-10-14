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
    ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMapV2,
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
/// [`ToolRegistration::presented_name_claimer`]: the flag is host authority,
/// never persisted, so reconstruction re-takes it here.
#[derive(Clone)]
pub struct ToolExecutorBindings {
    by_id: BTreeMap<ToolExecutorId, BoundToolExecutor>,
    claimers: BTreeSet<RegisteredToolId>,
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
        Self { by_id, claimers }
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
}

impl ToolExecutorBindingsBuilder {
    /// Creates an empty process-local binding builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            by_id: BTreeMap::new(),
            claimers: BTreeSet::new(),
        }
    }

    /// Flags the registered tool `tool_id` as a presented-name claimer on
    /// reconstruction, as [`ToolRegistration::presented_name_claimer`] does at
    /// registration.
    pub fn presented_name_claimer(&mut self, tool_id: RegisteredToolId) -> &mut Self {
        self.claimers.insert(tool_id);
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
            let restored = ToolRegistration::new(
                tool_id,
                executor_id,
                expected,
                registration.definition.clone(),
                Arc::clone(&binding.executor),
            )
            .with_nested_eligibility(registration.nested_eligibility);
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
    pub fn capture(map: &ToolProjectionMapV2) -> Result<Self, ToolDescriptorError> {
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
    ) -> Result<ToolProjectionMapV2, ToolDescriptorError> {
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
        let map = ToolProjectionMapV2::new(registry, self.wire.target.clone(), entries)
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
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use plakous_model::{
        AttemptId, CallId, DigestDomain, EffectiveProviderConfigurationId, ModelContentContractV1,
        PortableToolSchemaV1, PortableToolSchemaV2, RunId, ToolConcurrency, ToolCorrelation,
        ToolEffect, ToolInputContract, ToolOutputContract, TurnId, Utf8TextContractV1,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        ToolContext, ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome,
        ToolRuntimeErrorKind,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
    }

    impl ToolExecutor for CountingExecutor {
        fn execute(
            self: Arc<Self>,
            _invocation: ToolInvocation,
            _context: ToolContext,
        ) -> ToolFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { ToolOutcome::success_text("ok") })
        }
    }

    fn structured_definition(name: &str) -> TestResult<ToolDefinition> {
        Ok(ToolDefinition::new(
            ToolName::new(name)?,
            "1.2.3",
            format!("{name} description"),
            ToolInputContract::Structured(PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"value":{"type":"string"}},
                "required":["value"],
                "additionalProperties":false
            }))?),
            ToolOutputContract::Structured(PortableToolSchemaV1::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"result":{"type":"integer"}},
                "required":["result"],
                "additionalProperties":false
            }))?),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?)
    }

    fn utf8_definition(name: &str) -> TestResult<ToolDefinition> {
        Ok(ToolDefinition::new(
            ToolName::new(name)?,
            "2",
            "UTF-8 tool",
            ToolInputContract::Utf8(Utf8TextContractV1::new(4_096)?),
            ToolOutputContract::Utf8(Utf8TextContractV1::new(99)?),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )?)
    }

    fn model_content_definition(name: &str) -> TestResult<ToolDefinition> {
        Ok(ToolDefinition::new(
            ToolName::new(name)?,
            "3",
            "model content tool",
            ToolInputContract::Utf8(Utf8TextContractV1::new(55)?),
            ToolOutputContract::ModelContent(ModelContentContractV1::new(7, 777)?),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?)
    }

    fn registry_in_order(order: &[usize]) -> TestResult<(FrozenToolRegistry, Arc<AtomicUsize>)> {
        let calls = Arc::new(AtomicUsize::new(0));
        let executors = [
            Arc::new(CountingExecutor {
                calls: Arc::clone(&calls),
            }),
            Arc::new(CountingExecutor {
                calls: Arc::clone(&calls),
            }),
            Arc::new(CountingExecutor {
                calls: Arc::clone(&calls),
            }),
        ];
        let definitions = [
            structured_definition("alpha")?,
            utf8_definition("beta")?,
            model_content_definition("gamma")?,
        ];
        let limits = ToolRegistryLimits {
            max_tools: 9,
            max_total_schema_bytes: 32_000,
            schema: SchemaLimits { max_bytes: 8_000 },
            arguments: ArgumentLimits {
                max_bytes: 9_000,
                max_depth: 12,
                max_nodes: 500,
                max_string_bytes: 700,
            },
            reserved_prefixes: vec!["z.".into(), "a.".into(), "z.".into()],
        };
        let mut builder = ToolRegistryBuilder::new(limits);
        for &index in order {
            builder.register(ToolRegistration::new(
                RegisteredToolId::new(format!("tool-{index}"))?,
                ToolExecutorId::new(format!("executor-{index}"))?,
                test_binding(index)?,
                definitions[index].clone(),
                executors[index].clone(),
            ))?;
        }
        Ok((builder.freeze(), calls))
    }

    fn test_binding(index: usize) -> TestResult<ToolExecutorBindingIdentity> {
        Ok(ToolExecutorBindingIdentity::new(
            1,
            &[format!("executor-binding-{index}").as_bytes()],
        )?)
    }

    fn bindings(executor_count: usize) -> TestResult<ToolExecutorBindings> {
        bindings_with_calls(executor_count, &Arc::new(AtomicUsize::new(0)))
    }

    fn bindings_with_calls(
        executor_count: usize,
        calls: &Arc<AtomicUsize>,
    ) -> TestResult<ToolExecutorBindings> {
        let mut builder = ToolExecutorBindingsBuilder::new();
        for index in 0..executor_count {
            builder.bind(
                ToolExecutorId::new(format!("executor-{index}"))?,
                test_binding(index)?,
                Arc::new(CountingExecutor {
                    calls: Arc::clone(calls),
                }),
            )?;
        }
        Ok(builder.freeze())
    }

    fn product_plan_registry(session_id: &str) -> TestResult<FrozenToolRegistryDescriptor> {
        let authority = crate::standard_identity::PlanStateIdentity::new(
            format!("plak.session.{session_id}"),
            1,
        )?;
        let mut registry = ToolRegistryBuilder::default();
        registry.register(crate::standard_identity::expected_update_plan_registration(
            &authority.executor_id()?,
        )?)?;
        Ok(FrozenToolRegistryDescriptor::capture(&registry.freeze())?)
    }

    /// The presented-name claimer flag is host authority: reconstruction
    /// re-takes it from the live binding table, never from the persisted
    /// descriptor, and it moves no digest.
    #[test]
    fn reconstruction_retakes_the_claimer_flag_from_the_host_table() -> TestResult {
        let calls = Arc::new(AtomicUsize::new(0));
        let register = |claims: bool| -> TestResult<FrozenToolRegistry> {
            let mut builder = ToolRegistryBuilder::default();
            for (index, definition) in [structured_definition("alpha")?, utf8_definition("beta")?]
                .into_iter()
                .enumerate()
            {
                let registration = ToolRegistration::new(
                    RegisteredToolId::new(format!("tool-{index}"))?,
                    ToolExecutorId::new(format!("executor-{index}"))?,
                    test_binding(index)?,
                    definition,
                    Arc::new(CountingExecutor {
                        calls: Arc::clone(&calls),
                    }),
                );
                builder.register(if claims && index == 1 {
                    registration.presented_name_claimer()
                } else {
                    registration
                })?;
            }
            Ok(builder.freeze())
        };
        let live = register(true)?;
        let unflagged = register(false)?;
        let descriptor = FrozenToolRegistryDescriptor::capture(&live)?;
        assert_eq!(
            descriptor,
            FrozenToolRegistryDescriptor::capture(&unflagged)?,
            "the flag is in no persisted byte"
        );
        let decoded = FrozenToolRegistryDescriptor::decode_v2(&descriptor.encode_v2()?)?;

        let restored = decoded.resolve(&ToolExecutorBindings::capture(&live))?;
        assert_eq!(
            restored.registry_identity_digest()?,
            live.registry_identity_digest()?
        );
        for identity in live.identities() {
            assert_eq!(
                restored.identity(identity.name()),
                Some(identity),
                "live and restored identities compare equal, flag included"
            );
        }
        assert!(
            restored
                .identity(&ToolName::new("beta")?)
                .is_some_and(RegisteredToolIdentity::claims_presented_name)
        );

        let restored = decoded.resolve(&ToolExecutorBindings::capture(&unflagged))?;
        assert!(
            restored
                .identities()
                .all(|identity| !identity.claims_presented_name()),
            "a host that does not flag the registration confers no claim"
        );

        let mut builder = ToolExecutorBindingsBuilder::new();
        for index in 0..2 {
            builder.bind(
                ToolExecutorId::new(format!("executor-{index}"))?,
                test_binding(index)?,
                Arc::new(CountingExecutor {
                    calls: Arc::clone(&calls),
                }),
            )?;
        }
        builder.presented_name_claimer(RegisteredToolId::new("tool-1")?);
        let restored = decoded.resolve(&builder.freeze())?;
        assert_eq!(
            restored.identity(&ToolName::new("beta")?),
            live.identity(&ToolName::new("beta")?)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn registry_v1_is_order_independent_complete_and_reconstructive() -> TestResult {
        let (forward, _) = registry_in_order(&[0, 1, 2])?;
        let (reverse, _) = registry_in_order(&[2, 1, 0])?;
        let forward = FrozenToolRegistryDescriptor::capture(&forward)?;
        let reverse = FrozenToolRegistryDescriptor::capture(&reverse)?;
        let bytes = forward.encode_v1()?;
        assert_eq!(bytes, reverse.encode_v1()?);
        assert_eq!(FrozenToolRegistryDescriptor::decode_v1(&bytes)?, forward);

        let calls = Arc::new(AtomicUsize::new(0));
        let resolved = forward.resolve(&bindings_with_calls(4, &calls)?)?;
        assert_eq!(resolved.len(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(resolved.definition(&ToolName::new("extra")?).is_none());
        assert!(
            resolved
                .prepare(request("alpha", r#"{"value":"reconstructed"}"#)?)
                .is_ok()
        );
        assert_eq!(
            resolved
                .prepare(request("beta", "x".repeat(4_097))?)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::ResourceLimit)
        );
        assert_eq!(
            resolved.definition(&ToolName::new("alpha")?),
            Some(&structured_definition("alpha")?)
        );
        assert_eq!(
            resolved
                .definition(&ToolName::new("beta")?)
                .ok_or("beta missing")?
                .output(),
            utf8_definition("beta")?.output()
        );
        assert_eq!(
            resolved
                .definition(&ToolName::new("gamma")?)
                .ok_or("gamma missing")?
                .output(),
            model_content_definition("gamma")?.output()
        );
        Ok(())
    }

    #[test]
    fn registry_interface_identity_allows_executor_rebinding_only() -> TestResult {
        let (original, _) = registry_in_order(&[0, 1, 2])?;
        let mut rebound = ToolRegistryBuilder::new(original.limits().clone());
        for (index, definition) in original.definitions().iter().cloned().enumerate() {
            rebound.register(ToolRegistration::new(
                RegisteredToolId::new(format!("tool-{index}"))?,
                ToolExecutorId::new(format!("rebound-executor-{index}"))?,
                ToolExecutorBindingIdentity::new(
                    1,
                    &[format!("rebound-binding-{index}").as_bytes()],
                )?,
                definition,
                Arc::new(CountingExecutor {
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            ))?;
        }
        let original = FrozenToolRegistryDescriptor::capture(&original)?;
        let rebound = FrozenToolRegistryDescriptor::capture(&rebound.freeze())?;
        assert_ne!(original, rebound);
        assert_eq!(
            original.interface_identity()?,
            rebound.interface_identity()?
        );

        let (changed, _) = registry_in_order(&[0, 1])?;
        let changed = FrozenToolRegistryDescriptor::capture(&changed)?;
        assert_ne!(
            original.interface_identity()?,
            changed.interface_identity()?
        );
        Ok(())
    }

    #[test]
    fn canonical_authority_excludes_only_exact_composition_markers() -> TestResult {
        let empty =
            FrozenToolRegistryDescriptor::capture(&ToolRegistryBuilder::default().freeze())?;
        let mut markers = ToolRegistryBuilder::default();
        crate::composition_markers::register_product_javascript_markers_into(&mut markers)?;
        let markers = FrozenToolRegistryDescriptor::capture(&markers.freeze())?;
        assert_eq!(
            empty.canonical_authority_identity()?,
            markers.canonical_authority_identity()?
        );

        let mut spoofed = ToolRegistryBuilder::default();
        spoofed.register(ToolRegistration::new(
            RegisteredToolId::new("spoofed-composition-marker")?,
            ToolExecutorId::new(
                crate::composition_markers::PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1,
            )?,
            ToolExecutorBindingIdentity::new(1, &[b"spoofed-binding"])?,
            structured_definition("spoofed")?,
            Arc::new(CountingExecutor {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        ))?;
        let spoofed = FrozenToolRegistryDescriptor::capture(&spoofed.freeze())?;
        assert!(spoofed.canonical_authority_identity().is_err());
        Ok(())
    }

    #[test]
    fn canonical_authority_normalizes_only_exact_product_plan_tool_state_identity() -> TestResult {
        let first = product_plan_registry("6276f0c6-e24b-49ae-a630-6ced6b01cbad")?;
        let second = product_plan_registry("a9991d7d-28a8-47c3-8ce2-edc7d57871a1")?;
        assert_ne!(first, second);
        assert_eq!(
            first.canonical_authority_identity()?,
            second.canonical_authority_identity()?
        );

        let malformed = product_plan_registry("not-a-session-uuid")?;
        assert!(malformed.canonical_authority_identity().is_err());

        let exact_id = "plakous.standard.update-plan.v1.plak.session.\
            6276f0c6-e24b-49ae-a630-6ced6b01cbad.r1";
        let mut spoofed = ToolRegistryBuilder::default();
        spoofed.register(
            ToolRegistration::new(
                RegisteredToolId::new("plakous.standard.update-plan")?,
                ToolExecutorId::new(exact_id)?,
                ToolExecutorBindingIdentity::new(4, &[b"spoofed-binding"])?,
                structured_definition("update_plan")?,
                Arc::new(CountingExecutor {
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            )
            .nested_eligible(),
        )?;
        let spoofed = FrozenToolRegistryDescriptor::capture(&spoofed.freeze())?;
        assert!(spoofed.canonical_authority_identity().is_err());
        Ok(())
    }

    #[test]
    fn bindings_reject_duplicates_and_resolution_requires_every_exact_id() -> TestResult {
        let executor: Arc<dyn ToolExecutor> = Arc::new(CountingExecutor {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let mut duplicates = ToolExecutorBindingsBuilder::new();
        duplicates.bind(
            ToolExecutorId::new("executor-0")?,
            test_binding(0)?,
            Arc::clone(&executor),
        )?;
        assert!(matches!(
            duplicates.bind(ToolExecutorId::new("executor-0")?, test_binding(0)?, executor),
            Err(ToolBindingError::DuplicateBinding { id }) if id.as_str() == "executor-0"
        ));

        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
        assert!(matches!(
            descriptor.resolve(&bindings(2)?),
            Err(ToolBindingError::MissingBinding { id }) if id.as_str() == "executor-2"
        ));
        Ok(())
    }

    #[test]
    fn registry_codec_refuses_predecessor_unknown_trailing_noncanonical_and_tampering() -> TestResult
    {
        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
        let bytes = descriptor.encode_v1()?;

        let mut predecessor = bytes.clone();
        predecessor[8..12].copy_from_slice(&1_u32.to_be_bytes());
        assert_eq!(
            FrozenToolRegistryDescriptor::decode_v1(&predecessor),
            Err(ToolDescriptorError::UnsupportedVersion { found: 1 })
        );
        let mut unknown = bytes.clone();
        unknown[8..12].copy_from_slice(&3_u32.to_be_bytes());
        assert_eq!(
            FrozenToolRegistryDescriptor::decode_v1(&unknown),
            Err(ToolDescriptorError::UnsupportedVersion { found: 3 })
        );
        let mut trailing = bytes.clone();
        trailing.push(b' ');
        assert!(FrozenToolRegistryDescriptor::decode_v1(&trailing).is_err());

        let mut value: Value = serde_json::from_slice(&bytes[HEADER_BYTES..])?;
        value["unknown"] = json!(true);
        assert_eq!(
            FrozenToolRegistryDescriptor::decode_v1(&with_payload(&bytes, &value)?),
            Err(ToolDescriptorError::Malformed)
        );
        let mut missing: Value = serde_json::from_slice(&bytes[HEADER_BYTES..])?;
        missing
            .as_object_mut()
            .ok_or("registry payload is not object")?
            .remove("limits");
        assert_eq!(
            FrozenToolRegistryDescriptor::decode_v1(&with_payload(&bytes, &missing)?),
            Err(ToolDescriptorError::Malformed)
        );
        let mut tampered = descriptor.wire.clone();
        tampered.registrations[0].identity.semantic_version = "wrong".into();
        assert_eq!(
            FrozenToolRegistryDescriptor::decode_v1(&encode_envelope(
                REGISTRY_MAGIC,
                TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
                &tampered,
            )?),
            Err(ToolDescriptorError::IntegrityMismatch)
        );
        let mut eligibility_tampered = descriptor.wire.clone();
        eligibility_tampered.registrations[0]
            .identity
            .nested_eligibility = NestedToolEligibility::NestedEligible;
        assert_eq!(
            FrozenToolRegistryDescriptor::decode_v1(&encode_envelope(
                REGISTRY_MAGIC,
                TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
                &eligibility_tampered,
            )?),
            Err(ToolDescriptorError::IntegrityMismatch)
        );
        let mut reordered: Value = serde_json::from_slice(&bytes[HEADER_BYTES..])?;
        reordered["registrations"]
            .as_array_mut()
            .ok_or("registrations not array")?
            .swap(0, 1);
        assert_eq!(
            FrozenToolRegistryDescriptor::decode_v1(&with_payload(&bytes, &reordered)?),
            Err(ToolDescriptorError::NonCanonical)
        );
        Ok(())
    }

    #[test]
    fn exposure_v1_roundtrips_complete_mapping_and_revalidates_digest() -> TestResult {
        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let alpha = registry
            .identity(&ToolName::new("alpha")?)
            .ok_or("alpha missing")?
            .clone();
        let beta = registry
            .identity(&ToolName::new("beta")?)
            .ok_or("beta missing")?
            .clone();
        let exposure = ToolExposureMap::new(
            registry.clone(),
            [
                ToolExposure::new(beta, "visible_beta")?,
                ToolExposure::new(alpha, "visible_alpha")?
                    .with_description_override(Some("historical override".into()))?,
            ],
        )?;
        let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
        let exposure_descriptor = FrozenToolExposureDescriptor::capture(&exposure)?;
        let bytes = exposure_descriptor.encode_v1()?;
        let decoded = FrozenToolExposureDescriptor::decode_v1(&bytes, &registry_descriptor)?;
        let reconstructed = decoded.resolve(registry_descriptor.resolve(&bindings(3)?)?)?;
        assert_eq!(reconstructed.digest(), exposure.digest());
        assert_eq!(reconstructed.definitions(), exposure.definitions());
        assert_eq!(
            reconstructed.entries()[0].description_override(),
            Some("historical override")
        );

        let mut tampered = exposure_descriptor.wire.clone();
        tampered.entries[0].description_override = Some("changed".into());
        assert_eq!(
            FrozenToolExposureDescriptor::decode_v1(
                &encode_envelope(
                    EXPOSURE_MAGIC,
                    TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
                    &tampered,
                )?,
                &registry_descriptor,
            ),
            Err(ToolDescriptorError::IntegrityMismatch)
        );
        Ok(())
    }

    fn projection_target() -> TestResult<ProviderModelProjectionIdentityV2> {
        let effective_configuration: EffectiveProviderConfigurationId =
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"test-provider-configuration"],
            )?)?)?;
        Ok(ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            effective_configuration,
            2,
        )?)
    }

    fn projection_schema(property: &str) -> TestResult<PortableToolSchemaV2> {
        Ok(PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{property:{"type":"string"}},
            "required":[property],
            "additionalProperties":false
        }))?)
    }

    fn projection_map(registry: FrozenToolRegistry) -> TestResult<ToolProjectionMapV2> {
        projection_map_for_target(registry, projection_target()?)
    }

    fn projection_map_for_target(
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> TestResult<ToolProjectionMapV2> {
        let identity = registry
            .identity(&ToolName::new("beta")?)
            .ok_or("beta missing")?
            .clone();
        let output = PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"output":{"type":"string"}},
            "required":["output"],
            "additionalProperties":false
        }))?;
        let defaults = SemanticJsonV1::from_value(json!({}))?;
        let entries = [
            ToolProjectionExposureV2::new(
                [(crate::CanonicalProcessOperationV2::Start, identity.clone())],
                "exec_command",
                "start or yield",
                projection_schema("cmd")?,
                defaults.clone(),
                ToolProjectionLoweringV2::CodexExecCommand,
                output.clone(),
                ToolProjectionRaisingV2::ProcessResult,
                target.clone(),
            )?,
            ToolProjectionExposureV2::new(
                [(crate::CanonicalProcessOperationV2::Write, identity)],
                "write_stdin",
                "write or poll",
                projection_schema("chars")?,
                defaults,
                ToolProjectionLoweringV2::CodexWriteStdin,
                output,
                ToolProjectionRaisingV2::ProcessResult,
                target.clone(),
            )?,
        ];
        Ok(ToolProjectionMapV2::new(registry, target, entries)?)
    }

    #[test]
    fn compiled_request_view_preserves_every_exact_axis_and_v2_definition() -> TestResult {
        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let map = projection_map(registry)?;
        let compiled = map.compile_request_view_v2()?;

        assert_eq!(compiled.target().protocol(), map.target().protocol());
        assert_eq!(
            compiled.target().integration_profile(),
            map.target().integration_profile()
        );
        assert_eq!(compiled.target().family(), map.target().family());
        assert_eq!(
            compiled.target().requested_model(),
            map.target().requested_model()
        );
        assert_eq!(
            compiled.target().effective_model(),
            map.target().effective_model()
        );
        assert_eq!(
            compiled.target().effective_provider_configuration(),
            map.target().effective_provider_configuration()
        );
        assert_eq!(
            compiled.target().projection_revision(),
            map.target().projection_revision()
        );
        assert_eq!(compiled.registry_identity(), map.registry_digest());
        assert_eq!(compiled.exposure_map_identity(), map.digest());
        assert_eq!(compiled.definitions().len(), map.entries().len());
        for (definition, entry) in compiled.definitions().iter().zip(map.entries()) {
            assert_eq!(
                definition.canonical_name(),
                entry
                    .canonical_operations()
                    .values()
                    .next()
                    .ok_or("operation missing")?
                    .name()
            );
            assert_eq!(definition.exposed_name(), entry.exposed_name());
            assert_eq!(definition.description(), entry.description());
            assert_eq!(definition.input_schema(), entry.input_schema());
            assert_eq!(definition.output_schema(), entry.output_schema());
            assert_eq!(definition.source_identity(), entry.identity());
        }
        Ok(())
    }

    #[test]
    fn mixed_projection_descriptor_roundtrips_exact_custom_presentation() -> TestResult {
        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let beta = registry
            .identity(&ToolName::new("beta")?)
            .ok_or("beta missing")?
            .clone();
        let alpha = registry
            .identity(&ToolName::new("alpha")?)
            .ok_or("alpha missing")?
            .clone();
        let target = projection_target()?;
        let output = PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object", "properties":{"output":{"type":"string"}},
            "required":["output"], "additionalProperties":false
        }))?;
        let map = ToolProjectionMapV2::new(
            registry.clone(),
            target.clone(),
            [
                ToolProjectionExposureV2::new_custom(
                    [(crate::CanonicalProcessOperationV2::Start, beta)],
                    "exec",
                    "execute bounded source",
                    Utf8TextContractV1::new(4_096)?,
                    plakous_model::ProjectedCustomToolFormatV2::lark("start: /(?s).+/")?,
                    SemanticJsonV1::from_value(json!({}))?,
                    ToolProjectionLoweringV2::CanonicalPassthrough,
                    output.clone(),
                    ToolProjectionRaisingV2::Identity,
                    target.clone(),
                )?,
                ToolProjectionExposureV2::new(
                    [(crate::CanonicalProcessOperationV2::Poll, alpha)],
                    "wait",
                    "wait for completion",
                    projection_schema("id")?,
                    SemanticJsonV1::from_value(json!({}))?,
                    ToolProjectionLoweringV2::CanonicalPassthrough,
                    output,
                    ToolProjectionRaisingV2::Identity,
                    target,
                )?,
            ],
        )?;
        let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
        let descriptor = FrozenToolProjectionDescriptorV2::capture(&map)?;
        let encoded = descriptor.encode_v2()?;
        assert_eq!(
            u32::from_be_bytes(encoded[8..12].try_into()?),
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION
        );
        let decoded = FrozenToolProjectionDescriptorV2::decode_v2(&encoded, &registry_descriptor)?;
        let legacy_lowering = Digest::sha256(
            DigestDomain::ToolDefinition,
            2,
            &[
                b"plakous-tools/projection-program-v2",
                b"lowering",
                b"canonical-passthrough",
                map.entries()[0].defaults().digest().bytes(),
            ],
        )?;
        assert_ne!(map.entries()[0].lowering_identity(), legacy_lowering);
        let mut predecessor = descriptor.wire.clone();
        predecessor.entries[0].lowering_identity = legacy_lowering;
        assert_eq!(
            FrozenToolProjectionDescriptorV2::decode_v2(
                &encode_envelope(
                    EXPOSURE_V2_MAGIC,
                    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
                    &predecessor,
                )?,
                &registry_descriptor,
            ),
            Err(ToolDescriptorError::IntegrityMismatch)
        );
        let resolved = decoded.resolve(registry_descriptor.resolve(&bindings(3)?)?)?;
        assert_eq!(resolved.digest(), map.digest());
        let exec = resolved.by_exposed(&ToolName::new("exec")?).ok_or("exec")?;
        let ProjectedToolPresentationV2::CustomUtf8 { input, format } = exec.presentation() else {
            return Err("exec did not retain custom presentation".into());
        };
        assert_eq!(input.max_bytes(), 4_096);
        assert_eq!(format.definition(), "start: /(?s).+/");
        assert!(exec.input_schema().is_none());
        assert!(
            resolved
                .by_exposed(&ToolName::new("wait")?)
                .ok_or("wait")?
                .input_schema()
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn compiled_request_view_never_falls_back_across_target_drift() -> TestResult {
        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let exact = projection_map(registry.clone())?.compile_request_view_v2()?;
        let other_configuration: EffectiveProviderConfigurationId =
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"other-provider-configuration"],
            )?)?)?;
        let targets = [
            ProviderModelProjectionIdentityV2::new(
                plakous_model::IntegrationClass::Standard,
                "other.responses",
                "test.route",
                "test.profile.r1",
                "responses-family",
                "exact-model",
                "exact-model",
                projection_target()?.effective_provider_configuration(),
                2,
            )?,
            ProviderModelProjectionIdentityV2::new(
                plakous_model::IntegrationClass::Standard,
                "test.responses",
                "test.route",
                "other.profile.r1",
                "responses-family",
                "exact-model",
                "exact-model",
                projection_target()?.effective_provider_configuration(),
                2,
            )?,
            ProviderModelProjectionIdentityV2::new(
                plakous_model::IntegrationClass::Standard,
                "test.responses",
                "test.route",
                "test.profile.r1",
                "other-family",
                "exact-model",
                "exact-model",
                projection_target()?.effective_provider_configuration(),
                2,
            )?,
            ProviderModelProjectionIdentityV2::new(
                plakous_model::IntegrationClass::Standard,
                "test.responses",
                "test.route",
                "test.profile.r1",
                "responses-family",
                "other-model",
                "other-model",
                projection_target()?.effective_provider_configuration(),
                2,
            )?,
            ProviderModelProjectionIdentityV2::new(
                plakous_model::IntegrationClass::Standard,
                "test.responses",
                "test.route",
                "test.profile.r1",
                "responses-family",
                "exact-model",
                "exact-model",
                other_configuration,
                2,
            )?,
            ProviderModelProjectionIdentityV2::new(
                plakous_model::IntegrationClass::Standard,
                "test.responses",
                "test.route",
                "test.profile.r1",
                "responses-family",
                "exact-model",
                "exact-model",
                projection_target()?.effective_provider_configuration(),
                3,
            )?,
        ];
        for target in targets {
            let drifted =
                projection_map_for_target(registry.clone(), target)?.compile_request_view_v2()?;
            assert_ne!(drifted.target(), exact.target());
            assert_ne!(
                drifted.exposure_map_identity(),
                exact.exposure_map_identity()
            );
        }
        Ok(())
    }

    #[test]
    fn v2_projection_descriptor_roundtrips_multiple_names_to_one_executor() -> TestResult {
        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let map = projection_map(registry.clone())?;
        assert_eq!(map.entries().len(), 2);
        assert_eq!(
            map.entries()[0]
                .canonical_operations()
                .values()
                .next()
                .ok_or("operation missing")?
                .tool_id(),
            map.entries()[1]
                .canonical_operations()
                .values()
                .next()
                .ok_or("operation missing")?
                .tool_id()
        );
        assert_eq!(
            map.entries()[0]
                .canonical_operations()
                .values()
                .next()
                .ok_or("operation missing")?
                .executor_id(),
            map.entries()[1]
                .canonical_operations()
                .values()
                .next()
                .ok_or("operation missing")?
                .executor_id()
        );

        let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
        let registry_bytes = registry_descriptor.encode_v2()?;
        let registry_descriptor = FrozenToolRegistryDescriptor::decode_v2(&registry_bytes)?;
        let descriptor = FrozenToolProjectionDescriptorV2::capture(&map)?;
        let bytes = descriptor.encode_v2()?;
        let decoded = FrozenToolProjectionDescriptorV2::decode_v2(&bytes, &registry_descriptor)?;
        let resolved_registry = registry_descriptor.resolve(&bindings(3)?)?;
        let resolved = decoded.resolve(resolved_registry)?;
        assert_eq!(resolved.digest(), map.digest());
        assert_eq!(resolved.target(), map.target());
        assert!(matches!(
            resolved.prepare(request("alpha", r#"{"value":"x"}"#)?),
            Err(error) if error.kind() == ToolRuntimeErrorKind::UnknownTool
        ));
        assert!(matches!(
            resolved.prepare(request("beta", r#"{"value":"x"}"#)?),
            Err(error) if error.kind() == ToolRuntimeErrorKind::UnknownTool
        ));
        assert_eq!(
            resolved
                .prepare(request("exec_command", r#"{"cmd":"x"}"#)?)?
                .receipt()
                .ok_or("projected receipt missing")?
                .canonical_identity()
                .tool_id()
                .as_str(),
            "tool-1"
        );
        assert_eq!(
            resolved
                .prepare(request("write_stdin", r#"{"chars":"x"}"#)?)?
                .receipt()
                .ok_or("projected receipt missing")?
                .canonical_identity()
                .executor_id()
                .as_str(),
            "executor-1"
        );
        Ok(())
    }

    #[test]
    fn v2_projection_refuses_collision_unknown_target_revision_and_tampering() -> TestResult {
        let (registry, _) = registry_in_order(&[0, 1, 2])?;
        let map = projection_map(registry.clone())?;
        let duplicate = map.entries()[0].clone();
        assert!(
            ToolProjectionMapV2::new(
                registry.clone(),
                map.target().clone(),
                [duplicate.clone(), duplicate],
            )
            .is_err()
        );
        assert_eq!(
            map.prepare(request("unknown_projection", "{}")?)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::UnknownTool)
        );
        assert!(
            ProviderModelProjectionIdentityV2::new(
                plakous_model::IntegrationClass::Standard,
                "test.responses",
                "test.route",
                "test.profile.r1",
                "responses-family",
                "requested",
                "different-effective",
                map.target().effective_provider_configuration(),
                2,
            )
            .is_err()
        );

        let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
        let descriptor = FrozenToolProjectionDescriptorV2::capture(&map)?;
        let bytes = descriptor.encode_v2()?;
        let mut predecessor = bytes.clone();
        predecessor[8..12].copy_from_slice(&1_u32.to_be_bytes());
        assert_eq!(
            FrozenToolProjectionDescriptorV2::decode_v2(&predecessor, &registry_descriptor),
            Err(ToolDescriptorError::UnsupportedVersion { found: 1 })
        );
        let mut tampered = descriptor.wire.clone();
        tampered.entries[0].canonical_operations[0].executor_id = "foreign-executor".into();
        assert_eq!(
            FrozenToolProjectionDescriptorV2::decode_v2(
                &encode_envelope(
                    EXPOSURE_V2_MAGIC,
                    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
                    &tampered,
                )?,
                &registry_descriptor,
            ),
            Err(ToolDescriptorError::IntegrityMismatch)
        );
        // Entry order is presentation order, but one exposed name appears once.
        let mut duplicated = descriptor.wire.clone();
        let repeated = duplicated.entries[0].clone();
        duplicated.entries.push(repeated);
        assert_eq!(
            FrozenToolProjectionDescriptorV2::decode_v2(
                &encode_envelope(
                    EXPOSURE_V2_MAGIC,
                    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
                    &duplicated,
                )?,
                &registry_descriptor,
            ),
            Err(ToolDescriptorError::InvalidSemantics)
        );
        Ok(())
    }

    fn with_payload(original: &[u8], value: &Value) -> TestResult<Vec<u8>> {
        let mut bytes = original[..HEADER_BYTES].to_vec();
        bytes.extend_from_slice(&serde_json::to_vec(value)?);
        Ok(bytes)
    }

    fn request(name: &str, arguments: impl Into<Arc<str>>) -> TestResult<ToolInvocationRequest> {
        Ok(ToolInvocationRequest::new(
            RunId::from_str("00000000-0000-0000-0000-000000000001")?,
            TurnId::from_str("00000000-0000-0000-0000-000000000002")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000003")?,
            CallId::from_str("00000000-0000-0000-0000-000000000004")?,
            ToolName::new(name)?,
            ToolCorrelation::default(),
            arguments,
        ))
    }
}
