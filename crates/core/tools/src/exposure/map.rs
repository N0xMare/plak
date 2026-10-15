//! The frozen tool exposure map and the registry, exposure and definition digests.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use plakous_model::{Digest, DigestDomain, ToolDefinition, ToolName};

use crate::{
    FrozenToolRegistry, RegisteredToolIdentity, ToolInvocation, ToolInvocationRequest,
    ToolRuntimeError, ToolRuntimeErrorKind,
};

const REGISTRY_IDENTITY_DIGEST_VERSION: u32 = 9;
const EXPOSURE_MAP_DIGEST_VERSION: u32 = 8;

/// One requested provider-facing name for an exact registered tool identity.
///
/// Construction validates the portable exposed name. Registry ownership,
/// reserved namespaces, and collisions are validated when the exposure map is
/// frozen.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolExposure {
    canonical_identity: RegisteredToolIdentity,
    exposed_name: ToolName,
    description_override: Option<String>,
}

impl ToolExposure {
    /// Creates a requested exposure for one frozen registered identity.
    ///
    /// # Errors
    ///
    /// Returns a registration error when the exposed name is not a portable
    /// [`ToolName`].
    pub fn new(
        canonical_identity: RegisteredToolIdentity,
        exposed_name: impl Into<String>,
    ) -> Result<Self, ToolRuntimeError> {
        let exposed_name = ToolName::new(exposed_name.into()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Registration,
                "exposed tool name is invalid",
                &error,
            )
        })?;
        Ok(Self {
            canonical_identity,
            exposed_name,
            description_override: None,
        })
    }

    /// Applies an optional model-facing description without changing executable identity.
    ///
    /// # Errors
    ///
    /// Returns a registration error for an empty override. Full definition
    /// bounds are revalidated when the exposure map is frozen.
    pub fn with_description_override(
        mut self,
        description: Option<String>,
    ) -> Result<Self, ToolRuntimeError> {
        if description.as_deref().is_some_and(str::is_empty) {
            return Err(registration_error(
                "exposed tool description override is empty",
            ));
        }
        self.description_override = description;
        Ok(self)
    }

    /// Returns the exact canonical identity selected from a frozen registry.
    #[must_use]
    pub const fn canonical_identity(&self) -> &RegisteredToolIdentity {
        &self.canonical_identity
    }

    /// Returns the provider-facing name.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Returns the optional provider-facing description override.
    #[must_use]
    pub fn description_override(&self) -> Option<&str> {
        self.description_override.as_deref()
    }
}

impl fmt::Debug for ToolExposure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolExposure")
            .field("canonical_identity", &self.canonical_identity)
            .field("exposed_name", &self.exposed_name)
            .finish_non_exhaustive()
    }
}

struct FrozenExposureMap {
    registry: FrozenToolRegistry,
    entries: Arc<[ToolExposure]>,
    definitions: Arc<[ToolDefinition]>,
    by_canonical: BTreeMap<ToolName, usize>,
    by_exposed: BTreeMap<ToolName, usize>,
    registry_digest: Digest,
    digest: Digest,
}

/// Immutable per-run mapping between registered and provider-facing tool names.
///
/// The map may select a subset of a registry. Every selected entry remains
/// bound to its exact registry allocation and complete frozen identity. Its
/// deterministic digest binds the exact exposed set; unrelated unexposed
/// registry entries do not invalidate request prefixes or effect decisions.
#[derive(Clone)]
pub struct ToolExposureMap {
    inner: Arc<FrozenExposureMap>,
}

impl ToolExposureMap {
    /// Validates requested exposures and freezes their bidirectional lookup.
    ///
    /// Input order does not affect entry order or the resulting digest.
    /// Omitting a registry entry is allowed; profiles use omission to select a
    /// provider-facing subset.
    ///
    /// # Errors
    ///
    /// Returns a registration error for a missing or foreign canonical
    /// identity, duplicate canonical entry, exposed alias collision, or name in
    /// a namespace reserved by the source registry.
    pub fn new(
        registry: FrozenToolRegistry,
        exposures: impl IntoIterator<Item = ToolExposure>,
    ) -> Result<Self, ToolRuntimeError> {
        let mut entries = exposures.into_iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.canonical_identity
                .name()
                .cmp(right.canonical_identity.name())
        });

        let mut by_canonical = BTreeMap::new();
        let mut by_exposed = BTreeMap::new();
        let mut definitions = Vec::with_capacity(entries.len());
        for (index, exposure) in entries.iter().enumerate() {
            if !registry.owns_identity(&exposure.canonical_identity) {
                return Err(registration_error(
                    "exposed tool identity is missing from or foreign to the registry",
                ));
            }
            if registry.exposed_name_is_reserved(&exposure.exposed_name) {
                return Err(registration_error(
                    "exposed tool name uses a reserved namespace",
                ));
            }
            if by_canonical
                .insert(exposure.canonical_identity.name().clone(), index)
                .is_some()
            {
                return Err(registration_error("duplicate canonical tool exposure"));
            }
            if by_exposed
                .insert(exposure.exposed_name.clone(), index)
                .is_some()
            {
                return Err(registration_error("exposed tool alias collision"));
            }

            let canonical = registry
                .definition(exposure.canonical_identity.name())
                .ok_or_else(|| registration_error("exposed tool definition is missing"))?;
            definitions.push(exposed_definition(canonical, exposure)?);
        }

        let registry_digest = registry_identity_digest(&registry)?;
        let digest = exposure_map_digest(&entries, &definitions)?;
        Ok(Self {
            inner: Arc::new(FrozenExposureMap {
                registry,
                entries: entries.into(),
                definitions: definitions.into(),
                by_canonical,
                by_exposed,
                registry_digest,
                digest,
            }),
        })
    }

    /// Returns selected exposures in deterministic canonical-name order.
    #[must_use]
    pub fn entries(&self) -> &[ToolExposure] {
        &self.inner.entries
    }

    /// Returns provider-facing definitions aligned with [`Self::entries`].
    ///
    /// Name and an explicit description override may differ from the source;
    /// schema, semantic version, concurrency, and effect metadata are preserved.
    #[must_use]
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.inner.definitions
    }

    /// Returns the number of selected exposures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    /// Reports whether the provider-facing selection is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    /// Looks up an exposure by its exact registered identity.
    #[must_use]
    pub fn by_canonical(&self, identity: &RegisteredToolIdentity) -> Option<&ToolExposure> {
        if !self.inner.registry.owns_identity(identity) {
            return None;
        }
        self.inner
            .by_canonical
            .get(identity.name())
            .and_then(|index| self.inner.entries.get(*index))
            .filter(|entry| entry.canonical_identity == *identity)
    }

    /// Looks up an exposure by provider-facing name.
    #[must_use]
    pub fn by_exposed(&self, exposed_name: &ToolName) -> Option<&ToolExposure> {
        self.inner
            .by_exposed
            .get(exposed_name)
            .and_then(|index| self.inner.entries.get(*index))
    }

    /// Resolves an exact registered identity to its provider-facing name.
    #[must_use]
    pub fn exposed_name(&self, identity: &RegisteredToolIdentity) -> Option<&ToolName> {
        self.by_canonical(identity).map(ToolExposure::exposed_name)
    }

    /// Resolves a provider-facing name to its exact registered identity.
    #[must_use]
    pub fn canonical_identity(&self, exposed_name: &ToolName) -> Option<&RegisteredToolIdentity> {
        self.by_exposed(exposed_name)
            .map(ToolExposure::canonical_identity)
    }

    /// Returns the deterministic identity of the complete source registry.
    #[must_use]
    pub fn registry_digest(&self) -> Digest {
        self.inner.registry_digest
    }

    /// Reports whether this map was frozen from the exact executable registry
    /// supplied by the caller.
    ///
    /// This is stronger than digest equality and prevents a map from preparing
    /// invocations whose private provenance cannot be executed by another
    /// otherwise identical registry allocation.
    #[must_use]
    pub fn is_for_registry(&self, registry: &FrozenToolRegistry) -> bool {
        self.inner.registry.is_same_snapshot(registry)
    }

    /// Returns the request-prefix binding for this exact exposed tool set.
    ///
    /// The digest changes when a selected canonical identity/version, schema,
    /// executor, exposed definition, or exposed name changes. Unselected
    /// registry entries are intentionally excluded.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.inner.digest
    }

    pub(crate) fn registry(&self) -> &FrozenToolRegistry {
        &self.inner.registry
    }

    /// Resolves an exposed request and validates it against the canonical schema.
    ///
    /// The returned invocation retains the exact registered canonical identity;
    /// the provider-facing alias never becomes executable identity.
    ///
    /// # Errors
    ///
    /// Returns an unknown-tool error for an unexposed name, or the canonical
    /// registry's validation error for malformed or schema-invalid arguments.
    pub fn prepare(
        &self,
        request: ToolInvocationRequest,
    ) -> Result<ToolInvocation, ToolRuntimeError> {
        let exposed_name = request.tool().clone();
        let identity = self.canonical_identity(request.tool()).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::UnknownTool,
                "tool name is not exposed by this map",
            )
        })?;
        let invocation = self
            .inner
            .registry
            .prepare(request.with_tool(identity.name().clone()))?;
        Ok(invocation.with_exposure(exposed_name, self.digest()))
    }
}

impl fmt::Debug for ToolExposureMap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolExposureMap")
            .field("entry_count", &self.len())
            .field("registry_digest", &self.registry_digest())
            .field("digest", &self.digest())
            .finish()
    }
}

pub(crate) fn exposed_definition(
    canonical: &ToolDefinition,
    exposure: &ToolExposure,
) -> Result<ToolDefinition, ToolRuntimeError> {
    ToolDefinition::new(
        exposure.exposed_name.clone(),
        canonical.semantic_version(),
        exposure
            .description_override()
            .unwrap_or_else(|| canonical.description()),
        canonical.input().clone(),
        canonical.output().clone(),
        canonical.concurrency(),
        canonical.effect(),
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "exposed tool definition could not be constructed",
            &error,
        )
    })
}

pub(crate) fn registry_identity_digest(
    registry: &FrozenToolRegistry,
) -> Result<Digest, ToolRuntimeError> {
    let identities = registry.identities().cloned().collect::<Vec<_>>();
    registry_identity_digest_from_identities(&identities)
}

pub(crate) fn registry_identity_digest_from_identities(
    identities: &[RegisteredToolIdentity],
) -> Result<Digest, ToolRuntimeError> {
    let mut identities = identities.iter().collect::<Vec<_>>();
    identities.sort_by(|left, right| left.name().cmp(right.name()));
    let mut parts = vec![b"plakous-tools/frozen-registry".to_vec()];
    for identity in identities {
        push_identity_parts(&mut parts, identity);
    }
    digest_parts(
        DigestDomain::ToolDefinition,
        REGISTRY_IDENTITY_DIGEST_VERSION,
        &parts,
    )
}

pub(crate) fn exposure_map_digest(
    entries: &[ToolExposure],
    definitions: &[ToolDefinition],
) -> Result<Digest, ToolRuntimeError> {
    if entries.len() != definitions.len() {
        return Err(registration_error(
            "tool exposure identity inputs are not exactly aligned",
        ));
    }
    let mut aligned = entries.iter().zip(definitions).collect::<Vec<_>>();
    aligned.sort_by(|(left, _), (right, _)| {
        left.canonical_identity
            .name()
            .cmp(right.canonical_identity.name())
    });
    let mut parts = vec![b"plakous-tools/tool-exposure-map".to_vec()];
    for (exposure, definition) in aligned {
        push_identity_parts(&mut parts, &exposure.canonical_identity);
        parts.push(exposure.exposed_name.as_str().as_bytes().to_vec());
        parts.push(b"exposed-definition-digest/sha-256/tool-definition".to_vec());
        parts.push(definition.digest().version().to_be_bytes().to_vec());
        parts.push(definition.digest().bytes().to_vec());
    }
    digest_parts(
        DigestDomain::RequestPlan,
        EXPOSURE_MAP_DIGEST_VERSION,
        &parts,
    )
}

fn push_identity_parts(parts: &mut Vec<Vec<u8>>, identity: &RegisteredToolIdentity) {
    parts.push(b"registered-tool".to_vec());
    parts.push(identity.tool_id().as_str().as_bytes().to_vec());
    parts.push(identity.name().as_str().as_bytes().to_vec());
    parts.push(identity.semantic_version().as_bytes().to_vec());
    parts.push(b"definition-digest".to_vec());
    parts.push(identity.definition_digest().identity_bytes());
    parts.push(b"schema-digest".to_vec());
    parts.push(identity.schema_digest().identity_bytes());
    parts.push(b"argument-limits-digest".to_vec());
    parts.push(identity.argument_limits_digest().identity_bytes());
    parts.push(identity.executor_id().as_str().as_bytes().to_vec());
    parts.push(b"executor-binding-identity".to_vec());
    parts.push(
        identity
            .executor_binding_identity()
            .digest()
            .identity_bytes(),
    );
    parts.push(b"effect-metadata-digest".to_vec());
    parts.push(identity.effect_metadata_digest().identity_bytes());
    parts.push(b"nested-eligibility".to_vec());
    parts.push(identity.nested_eligibility().identity_bytes().to_vec());
}

fn digest_parts(
    domain: DigestDomain,
    version: u32,
    parts: &[Vec<u8>],
) -> Result<Digest, ToolRuntimeError> {
    let borrowed = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
    Digest::sha256(domain, version, &borrowed).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "tool exposure digest could not be computed",
            &error,
        )
    })
}

pub(super) fn registration_error(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::Registration, message)
}
