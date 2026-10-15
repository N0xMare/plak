//! Mutable registration followed by immutable executable snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_model::{
    Digest, DigestDomain, McpServerId, ToolDefinition, ToolEffect, ToolExecutionOwner, ToolName,
};

use crate::{
    ArgumentLimits, SchemaLimits, ToolExecutor, ToolInvocation, ToolInvocationRequest,
    ToolRuntimeError, ToolRuntimeErrorKind, ValidatedToolSchema,
};

/// Whether a canonical registered tool may be selected by a nested-composition profile.
///
/// Eligibility is inert registry data. It does not expose a tool, authorize an
/// invocation, or qualify any composition profile.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NestedToolEligibility {
    /// The tool may be invoked only through an independently admitted direct presentation.
    #[default]
    DirectOnly,
    /// An exact composition profile may consider the tool for its nested catalog.
    NestedEligible,
}

impl NestedToolEligibility {
    pub(crate) const fn identity_bytes(self) -> &'static [u8] {
        match self {
            Self::DirectOnly => b"direct-only",
            Self::NestedEligible => b"nested-eligible",
        }
    }
}

macro_rules! stable_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Creates a stable portable identifier.
            ///
            /// # Errors
            ///
            /// Returns a registration error for empty, oversized, or non-portable text.
            pub fn new(value: impl Into<Arc<str>>) -> Result<Self, ToolRuntimeError> {
                let value = value.into();
                let valid = !value.is_empty()
                    && value.len() <= 128
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
                    });
                if !valid {
                    return Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Registration,
                        concat!(stringify!($name), " is not a portable identifier"),
                    ));
                }
                Ok(Self(value))
            }

            /// Returns identifier text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(
    RegisteredToolId,
    "A stable application-scoped registered tool identity."
);
stable_id!(
    ToolExecutorId,
    "A stable identity for the executable implementation."
);

/// Maximum byte length of an [`McpServerName`].
///
/// The exposed MCP tool name `mcp__<server>__<tool>` must still fit the
/// common 64-byte provider function-name bound with room for the tool part.
pub const MAX_MCP_SERVER_NAME_BYTES: usize = 32;

/// A configured MCP server id: `[a-z][a-z0-9]{0,15}(_[a-z0-9]+)*`, at most
/// [`MAX_MCP_SERVER_NAME_BYTES`] bytes.
///
/// The grammar admits no `__`, so `mcp__<server>__<tool>` splits back into
/// its server and tool unambiguously, and the name is a valid identifier in
/// every composition language.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct McpServerName(McpServerId);

impl McpServerName {
    /// Validates one configured server id.
    ///
    /// # Errors
    ///
    /// Returns a registration error when `value` is outside the grammar.
    pub fn new(value: impl Into<String>) -> Result<Self, ToolRuntimeError> {
        let value = value.into();
        let invalid = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "MCP server id must match [a-z][a-z0-9]{0,15}(_[a-z0-9]+)* within 32 bytes",
            )
        };
        if !mcp_server_name_is_valid(&value) {
            return Err(invalid());
        }
        McpServerId::new(value).map(Self).map_err(|_| invalid())
    }

    /// Returns the id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns the canonical server id committed history records.
    #[must_use]
    pub const fn canonical(&self) -> &McpServerId {
        &self.0
    }
}

fn mcp_server_name_is_valid(value: &str) -> bool {
    if value.len() > MAX_MCP_SERVER_NAME_BYTES {
        return false;
    }
    let mut segments = value.split('_');
    let Some(head) = segments.next() else {
        return false;
    };
    let lower_alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let head_valid = head.len() <= 16
        && head.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && head.bytes().all(lower_alphanumeric);
    head_valid
        && segments.all(|segment| !segment.is_empty() && segment.bytes().all(lower_alphanumeric))
}

impl fmt::Debug for McpServerName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("McpServerName")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for McpServerName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

/// Who executes a registered tool, as committed history records it.
///
/// The driver stamps this owner onto every call it lands for the tool and
/// onto the tool's results. It is registration data, not executable identity:
/// it is in no registry, exposure or projection digest, and durable
/// reconstruction re-takes it from the live host bindings.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub enum ToolOwnerClass {
    /// Plak executes the tool in process or through its execution backend.
    #[default]
    PlakLocal,
    /// A configured MCP server executes the tool.
    Mcp(McpServerName),
}

impl ToolOwnerClass {
    /// Returns the canonical execution owner committed history records.
    #[must_use]
    pub fn execution_owner(&self) -> ToolExecutionOwner {
        match self {
            Self::PlakLocal => ToolExecutionOwner::Local,
            Self::Mcp(server) => ToolExecutionOwner::Mcp {
                server: server.canonical().clone(),
            },
        }
    }
}
/// Immutable content identity for one exact executable binding configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ToolExecutorBindingIdentity(Digest);

impl ToolExecutorBindingIdentity {
    /// Derives an exact binding identity from a semantic revision and complete components.
    ///
    /// # Errors
    ///
    /// Returns a registration error when the bounded digest cannot be constructed.
    pub fn new(semantic_revision: u32, components: &[&[u8]]) -> Result<Self, ToolRuntimeError> {
        let mut parts = Vec::with_capacity(components.len() + 1);
        parts.push(b"plakous-tools/executor-binding".as_slice());
        parts.extend_from_slice(components);
        Digest::sha256(DigestDomain::ToolDefinition, semantic_revision, &parts)
            .map(Self)
            .map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Registration,
                    "tool executor binding identity could not be computed",
                    &error,
                )
            })
    }

    /// Restores an already validated durable binding digest.
    #[must_use]
    pub const fn from_digest(digest: Digest) -> Self {
        Self(digest)
    }

    /// Returns the content-addressed binding digest.
    #[must_use]
    pub const fn digest(self) -> Digest {
        self.0
    }
}

/// Immutable identity captured when a tool is registered.
#[derive(Clone)]
pub struct RegisteredToolIdentity {
    pub(crate) tool_id: RegisteredToolId,
    pub(crate) name: ToolName,
    pub(crate) semantic_version: Arc<str>,
    pub(crate) definition_digest: Digest,
    pub(crate) schema_digest: Digest,
    pub(crate) argument_limits_digest: Digest,
    pub(crate) executor_id: ToolExecutorId,
    pub(crate) executor_binding_identity: ToolExecutorBindingIdentity,
    pub(crate) effect_metadata_digest: Digest,
    pub(crate) nested_eligibility: NestedToolEligibility,
    /// Set only for a registration its host flagged to claim the core menu
    /// entry of its own exposed name. Absent from durable identity digests: it
    /// authorizes construction, it is not a persisted semantic, and
    /// reconstruction re-takes it from the live host bindings.
    pub(crate) claims_presented_name: bool,
    /// Registration data like the claimer flag: in no digest and not part of
    /// identity equality; reconstruction re-takes it from the host bindings.
    pub(crate) owner: ToolOwnerClass,
    pub(crate) provenance: Arc<RegistryProvenance>,
}

impl PartialEq for RegisteredToolIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.tool_id == other.tool_id
            && self.name == other.name
            && self.semantic_version == other.semantic_version
            && self.definition_digest == other.definition_digest
            && self.schema_digest == other.schema_digest
            && self.argument_limits_digest == other.argument_limits_digest
            && self.executor_id == other.executor_id
            && self.executor_binding_identity == other.executor_binding_identity
            && self.effect_metadata_digest == other.effect_metadata_digest
            && self.nested_eligibility == other.nested_eligibility
            && self.claims_presented_name == other.claims_presented_name
    }
}

impl Eq for RegisteredToolIdentity {}

impl fmt::Debug for RegisteredToolIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegisteredToolIdentity")
            .field("tool_id", &self.tool_id)
            .field("name", &self.name)
            .field("semantic_version", &self.semantic_version)
            .field("definition_digest", &self.definition_digest)
            .field("schema_digest", &self.schema_digest)
            .field("argument_limits_digest", &self.argument_limits_digest)
            .field("executor_id", &self.executor_id)
            .field("executor_binding_identity", &self.executor_binding_identity)
            .field("effect_metadata_digest", &self.effect_metadata_digest)
            .field("nested_eligibility", &self.nested_eligibility)
            .field("claims_presented_name", &self.claims_presented_name)
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl RegisteredToolIdentity {
    /// Returns the application-scoped tool ID.
    #[must_use]
    pub const fn tool_id(&self) -> &RegisteredToolId {
        &self.tool_id
    }

    /// Returns the canonical model-facing name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the frozen semantic version.
    #[must_use]
    pub fn semantic_version(&self) -> &str {
        &self.semantic_version
    }

    /// Returns the complete canonical definition digest.
    #[must_use]
    pub const fn definition_digest(&self) -> Digest {
        self.definition_digest
    }

    /// Returns the canonical parameter-schema digest.
    #[must_use]
    pub const fn schema_digest(&self) -> Digest {
        self.schema_digest
    }

    /// Returns the complete portable argument-validation limit identity.
    #[must_use]
    pub const fn argument_limits_digest(&self) -> Digest {
        self.argument_limits_digest
    }

    /// Returns the frozen executor identity.
    #[must_use]
    pub const fn executor_id(&self) -> &ToolExecutorId {
        &self.executor_id
    }

    /// Returns the exact executable configuration identity.
    #[must_use]
    pub const fn executor_binding_identity(&self) -> ToolExecutorBindingIdentity {
        self.executor_binding_identity
    }

    /// Returns the digest binding the effect class and fixed recovery policy.
    #[must_use]
    pub const fn effect_metadata_digest(&self) -> Digest {
        self.effect_metadata_digest
    }

    /// Returns the frozen nested-composition eligibility value.
    ///
    /// This read-only value grants no exposure, authorization, or qualification.
    #[must_use]
    pub const fn nested_eligibility(&self) -> NestedToolEligibility {
        self.nested_eligibility
    }

    /// Whether this registration may claim the core menu entry of its own name.
    ///
    /// Only a registration its host flagged sets this (Plak flags its topology
    /// output and kill tools and pi's `wait` and `stop`). An unflagged
    /// capability of the same name is a namespace clash.
    #[must_use]
    pub const fn claims_presented_name(&self) -> bool {
        self.claims_presented_name
    }

    /// Returns who executes this tool, as committed history records it.
    #[must_use]
    pub const fn owner(&self) -> &ToolOwnerClass {
        &self.owner
    }

    pub(crate) const fn provenance(&self) -> &Arc<RegistryProvenance> {
        &self.provenance
    }
}

/// Hard budgets for building one registry snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRegistryLimits {
    /// Maximum registered tools.
    pub max_tools: usize,
    /// Maximum sum of canonical schema bytes.
    pub max_total_schema_bytes: usize,
    /// Per-tool schema limits.
    pub schema: SchemaLimits,
    /// Per-invocation argument limits.
    pub arguments: ArgumentLimits,
    /// Reserved canonical name prefixes.
    pub reserved_prefixes: Vec<String>,
}

impl Default for ToolRegistryLimits {
    fn default() -> Self {
        Self {
            max_tools: 128,
            max_total_schema_bytes: 2 * 1024 * 1024,
            schema: SchemaLimits::default(),
            arguments: ArgumentLimits::default(),
            reserved_prefixes: vec![String::from("plak."), String::from("system.")],
        }
    }
}

/// One executor and its explicit stable identities before registration.
#[derive(Clone)]
pub struct ToolRegistration {
    tool_id: RegisteredToolId,
    executor_id: ToolExecutorId,
    executor_binding_identity: ToolExecutorBindingIdentity,
    definition: ToolDefinition,
    executor: Arc<dyn ToolExecutor>,
    nested_eligibility: NestedToolEligibility,
    claims_presented_name: bool,
    owner: ToolOwnerClass,
}

impl fmt::Debug for ToolRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistration")
            .field("tool_id", &self.tool_id)
            .field("executor_id", &self.executor_id)
            .field("executor_binding_identity", &self.executor_binding_identity)
            .field("definition", &self.definition)
            .field("nested_eligibility", &self.nested_eligibility)
            .field("claims_presented_name", &self.claims_presented_name)
            .field("owner", &self.owner)
            .field("executor", &"dyn ToolExecutor")
            .finish()
    }
}

impl ToolRegistration {
    /// Creates an exact immutable definition/executor binding.
    #[must_use]
    pub fn new(
        tool_id: RegisteredToolId,
        executor_id: ToolExecutorId,
        executor_binding_identity: ToolExecutorBindingIdentity,
        definition: ToolDefinition,
        executor: Arc<dyn ToolExecutor>,
    ) -> Self {
        Self {
            tool_id,
            executor_id,
            executor_binding_identity,
            definition,
            executor,
            nested_eligibility: NestedToolEligibility::DirectOnly,
            claims_presented_name: false,
            owner: ToolOwnerClass::PlakLocal,
        }
    }

    /// Sets who executes this tool; the default is
    /// [`ToolOwnerClass::PlakLocal`].
    #[must_use]
    pub fn with_owner(mut self, owner: ToolOwnerClass) -> Self {
        self.owner = owner;
        self
    }

    /// Returns who executes this tool.
    #[must_use]
    pub const fn owner(&self) -> &ToolOwnerClass {
        &self.owner
    }

    /// Opts this registration into claiming the core menu entry of its own
    /// exposed name. The host is trusted with this: it is the host's statement
    /// that the capability answers for that slot.
    #[must_use]
    pub fn presented_name_claimer(mut self) -> Self {
        self.claims_presented_name = true;
        self
    }

    /// Whether [`Self::presented_name_claimer`] was set.
    #[must_use]
    pub const fn claims_presented_name(&self) -> bool {
        self.claims_presented_name
    }

    /// Returns the canonical name carried by this registration.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        self.definition.name()
    }

    /// Returns the canonical definition this registration enrolls.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Explicitly opts this canonical capability into nested-catalog consideration.
    ///
    /// This marker grants no exposure, authorization, or profile qualification.
    #[must_use]
    pub fn nested_eligible(mut self) -> Self {
        self.nested_eligibility = NestedToolEligibility::NestedEligible;
        self
    }

    /// Explicitly constrains this registration to direct invocation only.
    ///
    /// This is useful when a caller starts from a reusable registration that
    /// opted in but the enclosing catalog must fail closed.
    #[must_use]
    pub fn direct_only(mut self) -> Self {
        self.nested_eligibility = NestedToolEligibility::DirectOnly;
        self
    }

    pub(crate) fn with_nested_eligibility(self, eligibility: NestedToolEligibility) -> Self {
        match eligibility {
            NestedToolEligibility::DirectOnly => self.direct_only(),
            NestedToolEligibility::NestedEligible => self.nested_eligible(),
        }
    }

    /// Returns the registration's explicit nested-composition eligibility.
    #[must_use]
    pub const fn nested_eligibility(&self) -> NestedToolEligibility {
        self.nested_eligibility
    }

    /// Reports whether this registration explicitly permits nested selection.
    #[must_use]
    pub const fn is_nested_eligible(&self) -> bool {
        matches!(
            self.nested_eligibility,
            NestedToolEligibility::NestedEligible
        )
    }
}

struct RegistryEntry {
    definition: ToolDefinition,
    identity: RegisteredToolIdentity,
    schema: ValidatedToolSchema,
    executor: Arc<dyn ToolExecutor>,
}

impl fmt::Debug for RegistryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryEntry")
            .field("name", &self.definition.name())
            .field("definition_digest", &self.definition.digest())
            .field("identity", &self.identity)
            .field("schema", &self.schema)
            .field("executor", &"dyn ToolExecutor")
            .finish()
    }
}

/// Unforgeable in-process identity for one exact frozen registry allocation.
#[derive(Debug)]
pub(crate) struct RegistryProvenance;

/// Mutable registration surface that consumes itself to produce a frozen registry.
#[derive(Debug)]
pub struct ToolRegistryBuilder {
    limits: ToolRegistryLimits,
    entries: Vec<RegistryEntry>,
    names: BTreeSet<ToolName>,
    tool_ids: BTreeSet<RegisteredToolId>,
    executor_bindings: BTreeMap<ToolExecutorId, usize>,
    total_schema_bytes: usize,
    provenance: Arc<RegistryProvenance>,
}

impl ToolRegistryBuilder {
    /// Creates an empty registry builder with explicit hard limits.
    #[must_use]
    pub fn new(limits: ToolRegistryLimits) -> Self {
        Self {
            limits,
            entries: Vec::new(),
            names: BTreeSet::new(),
            tool_ids: BTreeSet::new(),
            executor_bindings: BTreeMap::new(),
            total_schema_bytes: 0,
            provenance: Arc::new(RegistryProvenance),
        }
    }

    /// Registers and freezes one definition/schema/executor identity tuple.
    ///
    /// # Errors
    ///
    /// Rejects duplicate/reserved names, duplicate tool IDs, executor-ID
    /// aliases to a different capability, invalid schemas, and registry budget
    /// overflow.
    #[allow(
        clippy::too_many_lines,
        reason = "registration validates one atomic definition/schema/executor binding transaction"
    )]
    pub fn register(&mut self, registration: ToolRegistration) -> Result<(), ToolRuntimeError> {
        if self.entries.len() >= self.limits.max_tools {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool registry exceeds its tool-count limit",
            ));
        }
        let definition = registration.definition;
        if self
            .limits
            .reserved_prefixes
            .iter()
            .any(|prefix| definition.name().as_str().starts_with(prefix))
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "tool name uses a reserved namespace",
            ));
        }
        if self.names.contains(definition.name()) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "duplicate tool name",
            ));
        }
        if self.tool_ids.contains(&registration.tool_id) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "duplicate registered tool ID",
            ));
        }
        if self
            .executor_bindings
            .get(&registration.executor_id)
            .and_then(|index| self.entries.get(*index))
            .is_some_and(|bound| !Arc::ptr_eq(&bound.executor, &registration.executor))
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "tool executor ID is already bound to a different executor",
            ));
        }
        if self
            .executor_bindings
            .get(&registration.executor_id)
            .and_then(|index| self.entries.get(*index))
            .is_some_and(|bound| {
                bound.identity.executor_binding_identity() != registration.executor_binding_identity
            })
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "tool executor ID is already bound to a different binding identity",
            ));
        }

        let schema =
            ValidatedToolSchema::compile(&definition, self.limits.schema, self.limits.arguments)?;
        let effect_metadata_digest = effect_metadata_digest(definition.effect())?;
        let argument_limits_digest = argument_limits_digest(self.limits.arguments)?;
        let schema_bytes = match definition.input() {
            plakous_model::ToolInputContract::Structured(schema) => {
                schema.document().canonical_bytes().len()
            }
            plakous_model::ToolInputContract::Utf8(_) => 0,
            _ => {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "unsupported input contract revision",
                ));
            }
        };
        let next_total = self.total_schema_bytes.saturating_add(schema_bytes);
        if next_total > self.limits.max_total_schema_bytes {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool registry exceeds its total schema-byte limit",
            ));
        }
        let identity = RegisteredToolIdentity {
            tool_id: registration.tool_id.clone(),
            name: definition.name().clone(),
            semantic_version: Arc::from(definition.semantic_version()),
            definition_digest: definition.digest(),
            schema_digest: schema.schema_digest(),
            argument_limits_digest,
            executor_id: registration.executor_id.clone(),
            executor_binding_identity: registration.executor_binding_identity,
            effect_metadata_digest,
            nested_eligibility: registration.nested_eligibility,
            claims_presented_name: registration.claims_presented_name,
            owner: registration.owner,
            provenance: Arc::clone(&self.provenance),
        };
        self.names.insert(definition.name().clone());
        self.tool_ids.insert(registration.tool_id);
        self.executor_bindings
            .entry(registration.executor_id)
            .or_insert(self.entries.len());
        self.total_schema_bytes = next_total;
        self.entries.push(RegistryEntry {
            definition,
            identity,
            schema,
            executor: registration.executor,
        });
        Ok(())
    }

    /// Consumes mutable registration state and returns an immutable snapshot.
    #[must_use]
    pub fn freeze(self) -> FrozenToolRegistry {
        let mut by_name = BTreeMap::new();
        let mut definitions = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            by_name.insert(entry.definition.name().clone(), index);
            definitions.push(entry.definition.clone());
        }
        FrozenToolRegistry {
            inner: Arc::new(FrozenRegistry {
                entries: self.entries,
                by_name,
                definitions: definitions.into(),
                provenance: self.provenance,
                reserved_prefixes: self.limits.reserved_prefixes.clone().into(),
                limits: self.limits,
            }),
        }
    }
}

impl Default for ToolRegistryBuilder {
    fn default() -> Self {
        Self::new(ToolRegistryLimits::default())
    }
}

#[derive(Debug)]
struct FrozenRegistry {
    entries: Vec<RegistryEntry>,
    by_name: BTreeMap<ToolName, usize>,
    definitions: Arc<[ToolDefinition]>,
    provenance: Arc<RegistryProvenance>,
    reserved_prefixes: Arc<[String]>,
    limits: ToolRegistryLimits,
}

/// Cheaply cloneable immutable executable registry snapshot.
///
/// This value retains trusted native executor capabilities. Applications may
/// use [`FrozenToolRegistry::definitions`] for a non-executable model-facing
/// view, but must not hand the registry itself across an untrusted boundary.
#[derive(Clone, Debug)]
pub struct FrozenToolRegistry {
    inner: Arc<FrozenRegistry>,
}

impl FrozenToolRegistry {
    /// Reports whether two handles retain the exact same frozen executable
    /// registry authority.
    ///
    /// Equal definitions or stable IDs are insufficient: prepared invocations
    /// are executable only through the allocation whose private provenance they
    /// retain.
    #[must_use]
    pub fn is_same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns frozen canonical definitions in registration order.
    #[must_use]
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.inner.definitions
    }

    /// Returns the number of registered executors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    /// Returns the content-addressed identity of this frozen registry snapshot.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when identity encoding exceeds digest bounds.
    pub fn registry_identity_digest(&self) -> Result<Digest, ToolRuntimeError> {
        crate::exposure::registry_identity_digest(self)
    }

    /// Reports whether the snapshot has no registrations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    /// Returns frozen identity by canonical name.
    #[must_use]
    pub fn identity(&self, name: &ToolName) -> Option<&RegisteredToolIdentity> {
        self.entry(name).map(|entry| &entry.identity)
    }

    /// Returns one canonical tool's frozen nested-composition eligibility.
    ///
    /// This query is descriptive only and creates no invocation authority.
    #[must_use]
    pub fn nested_eligibility(&self, name: &ToolName) -> Option<NestedToolEligibility> {
        self.identity(name)
            .map(RegisteredToolIdentity::nested_eligibility)
    }

    /// Returns the frozen canonical definition for one registered name.
    #[must_use]
    pub fn definition(&self, name: &ToolName) -> Option<&ToolDefinition> {
        self.entry(name).map(|entry| &entry.definition)
    }

    /// Strictly parses, schema-validates, and identity-binds an invocation.
    ///
    /// # Errors
    ///
    /// Returns an unknown-tool, invalid-argument, or resource-limit error before
    /// any executor is called.
    pub fn prepare(
        &self,
        request: ToolInvocationRequest,
    ) -> Result<ToolInvocation, ToolRuntimeError> {
        let entry = self.entry(request.tool()).ok_or_else(|| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::UnknownTool, "tool is not registered")
        })?;
        let arguments = entry.schema.validate_arguments(request.raw_arguments())?;
        Ok(ToolInvocation::validated(
            request,
            entry.identity.clone(),
            arguments,
            Arc::clone(&self.inner.provenance),
        ))
    }

    pub(crate) fn executor(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<Arc<dyn ToolExecutor>, ToolRuntimeError> {
        if !Arc::ptr_eq(&self.inner.provenance, invocation.provenance()) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "prepared invocation belongs to a different frozen registry",
            ));
        }
        let entry = self.entry(invocation.identity().name()).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "prepared tool is absent from its frozen registry",
            )
        })?;
        if entry.identity != *invocation.identity() {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "prepared tool identity does not match its frozen registry",
            ));
        }
        Ok(Arc::clone(&entry.executor))
    }

    pub(crate) fn owns_identity(&self, identity: &RegisteredToolIdentity) -> bool {
        Arc::ptr_eq(&self.inner.provenance, identity.provenance())
            && self
                .entry(identity.name())
                .is_some_and(|entry| entry.identity == *identity)
    }

    pub(crate) fn identities(&self) -> impl ExactSizeIterator<Item = &RegisteredToolIdentity> {
        self.inner.entries.iter().map(|entry| &entry.identity)
    }

    pub(crate) fn durable_registrations(
        &self,
    ) -> impl ExactSizeIterator<Item = (&RegisteredToolIdentity, &ToolDefinition)> {
        self.inner
            .entries
            .iter()
            .map(|entry| (&entry.identity, &entry.definition))
    }

    pub(crate) fn durable_executor_bindings(
        &self,
    ) -> impl Iterator<
        Item = (
            &ToolExecutorId,
            ToolExecutorBindingIdentity,
            &Arc<dyn ToolExecutor>,
        ),
    > {
        self.inner.entries.iter().map(|entry| {
            (
                entry.identity.executor_id(),
                entry.identity.executor_binding_identity(),
                &entry.executor,
            )
        })
    }

    pub(crate) fn limits(&self) -> &ToolRegistryLimits {
        &self.inner.limits
    }

    pub(crate) fn exposed_name_is_reserved(&self, name: &ToolName) -> bool {
        self.inner
            .reserved_prefixes
            .iter()
            .any(|prefix| name.as_str().starts_with(prefix))
    }

    fn entry(&self, name: &ToolName) -> Option<&RegistryEntry> {
        self.inner
            .by_name
            .get(name)
            .and_then(|index| self.inner.entries.get(*index))
    }
}

pub(crate) fn effect_metadata_digest(effect: ToolEffect) -> Result<Digest, ToolRuntimeError> {
    let (effect_bytes, recovery_bytes) = match effect {
        ToolEffect::ReadOnly => (b"read-only".as_slice(), b"not-applicable".as_slice()),
        ToolEffect::Mutating => (b"mutating".as_slice(), b"fixed-manual-no-key-v1".as_slice()),
        _ => (b"unsupported".as_slice(), b"unsupported".as_slice()),
    };
    Digest::sha256(
        DigestDomain::ToolDefinition,
        2,
        &[
            b"plakous-tools/effect-metadata",
            effect_bytes,
            recovery_bytes,
        ],
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "tool effect metadata could not be digested",
            &error,
        )
    })
}

pub(crate) fn argument_limits_digest(limits: ArgumentLimits) -> Result<Digest, ToolRuntimeError> {
    let max_bytes = u64::try_from(limits.max_bytes).map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "argument byte limit is not portable",
        )
    })?;
    let max_depth = u64::try_from(limits.max_depth).map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "argument depth limit is not portable",
        )
    })?;
    let max_nodes = u64::try_from(limits.max_nodes).map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "argument node limit is not portable",
        )
    })?;
    let max_string_bytes = u64::try_from(limits.max_string_bytes).map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "argument string limit is not portable",
        )
    })?;
    Digest::sha256(
        DigestDomain::ToolDefinition,
        1,
        &[
            b"plakous-tools/argument-limits-v1",
            &max_bytes.to_be_bytes(),
            &max_depth.to_be_bytes(),
            &max_nodes.to_be_bytes(),
            &max_string_bytes.to_be_bytes(),
        ],
    )
    .map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "argument limits could not be bound",
        )
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use plakous_model::{
        SemanticJsonV1, ToolConcurrency, ToolEffect, ToolInputContract, ToolOutputContract,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::ToolOutcome;

    type TestTool = (ToolDefinition, Arc<crate::test_support::FixedTool>);

    fn tool(name: &str) -> Result<TestTool, Box<dyn std::error::Error>> {
        tool_with_schema(
            name,
            json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
        )
    }

    fn tool_with_schema(name: &str, schema: Value) -> Result<TestTool, Box<dyn std::error::Error>> {
        let definition = ToolDefinition::new(
            ToolName::new(name)?,
            "1.0.0",
            "A test tool.",
            ToolInputContract::structured(schema)?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?;
        Ok((
            definition,
            Arc::new(crate::test_support::FixedTool::new(
                ToolOutcome::success_text("ok")?,
            )),
        ))
    }

    fn registration(
        tool_id: &str,
        executor_id: &str,
        tool: TestTool,
    ) -> Result<ToolRegistration, Box<dyn std::error::Error>> {
        Ok(ToolRegistration::new(
            RegisteredToolId::new(tool_id)?,
            ToolExecutorId::new(executor_id)?,
            ToolExecutorBindingIdentity::new(1, &[executor_id.as_bytes()])?,
            tool.0,
            tool.1,
        ))
    }

    fn binding(value: &str) -> Result<ToolExecutorBindingIdentity, ToolRuntimeError> {
        ToolExecutorBindingIdentity::new(1, &[value.as_bytes()])
    }

    #[test]
    fn mcp_server_name_grammar_table() {
        for valid in [
            "a",
            "github",
            "gh2",
            "abcdefghijklmnop",
            "my_server",
            "a_b_c",
            "linear_2",
            "abcdefghijklmnop_qrstuvwxyz01234",
        ] {
            assert!(McpServerName::new(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "Github",
            "2fa",
            "_github",
            "github_",
            "git__hub",
            "git-hub",
            "git.hub",
            "abcdefghijklmnopq",
            "abcdefghijklmnop_qrstuvwxyz012345",
            "gith\u{fc}b",
        ] {
            assert_eq!(
                McpServerName::new(invalid).err().map(|error| error.kind()),
                Some(ToolRuntimeErrorKind::Registration),
                "{invalid}"
            );
        }
    }

    #[test]
    fn owner_class_defaults_local_stamps_results_and_moves_no_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::str::FromStr;

        let github = ToolOwnerClass::Mcp(McpServerName::new("github")?);
        assert_eq!(
            github.execution_owner(),
            ToolExecutionOwner::Mcp {
                server: McpServerId::new("github")?
            }
        );
        assert_eq!(
            ToolOwnerClass::default().execution_owner(),
            ToolExecutionOwner::Local
        );
        let freeze = |owner: Option<ToolOwnerClass>| -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
            let registration = registration("lookup-id", "lookup-exec", tool("lookup")?)?;
            assert_eq!(registration.owner(), &ToolOwnerClass::PlakLocal);
            let mut builder = ToolRegistryBuilder::default();
            builder.register(match owner {
                Some(owner) => registration.with_owner(owner),
                None => registration,
            })?;
            Ok(builder.freeze())
        };
        let local = freeze(None)?;
        let mcp = freeze(Some(github.clone()))?;
        assert_eq!(
            local.registry_identity_digest()?,
            mcp.registry_identity_digest()?
        );
        let name = ToolName::new("lookup")?;
        assert_eq!(
            mcp.identity(&name).map(RegisteredToolIdentity::owner),
            Some(&github)
        );

        let result = |registry: &FrozenToolRegistry| -> Result<ToolExecutionOwner, Box<dyn std::error::Error>> {
            let invocation = registry.prepare(ToolInvocationRequest::new(
                plakous_model::RunId::from_str("00000000-0000-0000-0000-000000000001")?,
                plakous_model::TurnId::from_str("00000000-0000-0000-0000-000000000002")?,
                plakous_model::AttemptId::from_str("00000000-0000-0000-0000-000000000003")?,
                plakous_model::CallId::from_str("00000000-0000-0000-0000-000000000004")?,
                name.clone(),
                plakous_model::ToolCorrelation::default(),
                "{}",
            ))?;
            Ok(ToolOutcome::success_text("ok")?
                .to_model_result(&invocation)?
                .owner()
                .clone())
        };
        assert_eq!(result(&local)?, ToolExecutionOwner::Local);
        assert_eq!(result(&mcp)?, github.execution_owner());
        Ok(())
    }

    #[test]
    fn duplicate_name_fails_in_isolation() -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = ToolRegistryBuilder::default();
        builder.register(registration("lookup-id", "lookup-exec", tool("lookup")?)?)?;
        let duplicate = builder.register(registration("other-id", "other-exec", tool("lookup")?)?);
        assert_eq!(
            duplicate.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );

        assert_eq!(builder.freeze().len(), 1);
        Ok(())
    }

    #[test]
    fn nested_eligibility_defaults_closed_and_rotates_registry_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut direct = ToolRegistryBuilder::default();
        direct.register(registration("lookup-id", "lookup-exec", tool("lookup")?)?)?;
        let direct = direct.freeze();
        let name = ToolName::new("lookup")?;
        assert_eq!(
            direct.nested_eligibility(&name),
            Some(NestedToolEligibility::DirectOnly)
        );

        let mut nested = ToolRegistryBuilder::default();
        nested.register(
            registration("lookup-id", "lookup-exec", tool("lookup")?)?.nested_eligible(),
        )?;
        let nested = nested.freeze();
        assert_eq!(
            nested.nested_eligibility(&name),
            Some(NestedToolEligibility::NestedEligible)
        );
        assert_ne!(direct.identity(&name), nested.identity(&name));
        assert_ne!(
            direct.registry_identity_digest()?,
            nested.registry_identity_digest()?
        );
        Ok(())
    }

    #[test]
    fn duplicate_tool_id_fails_in_isolation() -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = ToolRegistryBuilder::default();
        builder.register(registration("stable-id", "first-exec", tool("first")?)?)?;
        let duplicate =
            builder.register(registration("stable-id", "second-exec", tool("second")?)?);
        assert_eq!(
            duplicate.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );
        assert_eq!(builder.freeze().len(), 1);
        Ok(())
    }

    #[test]
    fn executor_id_alias_requires_the_exact_shared_binding()
    -> Result<(), Box<dyn std::error::Error>> {
        let (first, executor) = tool("first")?;
        let (second, different_executor) = tool("second")?;
        let mut rejected = ToolRegistryBuilder::default();
        rejected.register(ToolRegistration::new(
            RegisteredToolId::new("first-id")?,
            ToolExecutorId::new("shared-exec")?,
            binding("shared-exec")?,
            first.clone(),
            executor.clone(),
        ))?;
        let alias = rejected.register(ToolRegistration::new(
            RegisteredToolId::new("second-id")?,
            ToolExecutorId::new("shared-exec")?,
            binding("shared-exec")?,
            second.clone(),
            different_executor,
        ));
        assert_eq!(
            alias.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );
        assert_eq!(rejected.freeze().len(), 1);

        let shared: Arc<dyn ToolExecutor> = executor;
        let mut accepted = ToolRegistryBuilder::default();
        accepted.register(ToolRegistration::new(
            RegisteredToolId::new("first-id")?,
            ToolExecutorId::new("shared-exec")?,
            binding("shared-exec")?,
            first,
            Arc::clone(&shared),
        ))?;
        accepted.register(ToolRegistration::new(
            RegisteredToolId::new("second-id")?,
            ToolExecutorId::new("shared-exec")?,
            binding("shared-exec")?,
            second,
            shared,
        ))?;
        assert_eq!(accepted.freeze().len(), 2);
        Ok(())
    }

    #[test]
    fn reserved_name_fails_in_isolation() -> Result<(), Box<dyn std::error::Error>> {
        let mut reserved = ToolRegistryBuilder::default();
        assert!(
            reserved
                .register(registration(
                    "reserved-id",
                    "reserved-exec",
                    tool("plak.internal")?,
                )?)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn max_tools_overflow_is_typed_and_error_atomic() -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits {
            max_tools: 1,
            ..ToolRegistryLimits::default()
        });
        builder.register(registration("first-id", "first-exec", tool("first")?)?)?;

        let error = builder
            .register(registration(
                "rejected-id",
                "rejected-exec",
                tool("rejected")?,
            )?)
            .err()
            .ok_or("tool-count overflow was accepted")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::ResourceLimit);
        assert_eq!(
            error.to_string(),
            "tool registry exceeds its tool-count limit"
        );

        let frozen = builder.freeze();
        assert_eq!(frozen.len(), 1);
        assert!(frozen.identity(&ToolName::new("first")?).is_some());
        assert!(frozen.identity(&ToolName::new("rejected")?).is_none());
        Ok(())
    }

    #[test]
    fn max_total_schema_bytes_overflow_is_typed_and_error_atomic()
    -> Result<(), Box<dyn std::error::Error>> {
        let small_schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false});
        let rejected_schema = json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"payload":{"type":"string"}},
            "required":["payload"],
            "additionalProperties":false
        });
        let small_bytes = SemanticJsonV1::from_value(small_schema.clone())?
            .canonical_bytes()
            .len();
        assert!(
            SemanticJsonV1::from_value(rejected_schema.clone())?
                .canonical_bytes()
                .len()
                > small_bytes
        );
        let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits {
            max_tools: 3,
            max_total_schema_bytes: small_bytes.saturating_mul(2),
            ..ToolRegistryLimits::default()
        });
        builder.register(registration(
            "first-id",
            "first-exec",
            tool_with_schema("first", small_schema.clone())?,
        )?)?;

        let error = builder
            .register(registration(
                "rejected-id",
                "rejected-exec",
                tool_with_schema("rejected", rejected_schema)?,
            )?)
            .err()
            .ok_or("total schema-byte overflow was accepted")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::ResourceLimit);
        assert_eq!(
            error.to_string(),
            "tool registry exceeds its total schema-byte limit"
        );

        builder.register(registration(
            "third-id",
            "third-exec",
            tool_with_schema("third", small_schema)?,
        )?)?;
        let frozen = builder.freeze();
        assert_eq!(frozen.len(), 2);
        assert_eq!(
            frozen
                .definitions()
                .iter()
                .map(|definition| definition.name().as_str())
                .collect::<Vec<_>>(),
            ["first", "third"]
        );
        assert!(frozen.identity(&ToolName::new("rejected")?).is_none());
        Ok(())
    }

    #[test]
    fn executor_has_no_descriptor_authority_and_revision_drift_fails_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let (first, executor) = tool("first")?;
        let mut drifted = first.clone();
        drifted = ToolDefinition::new(
            drifted.name().clone(),
            "2.0.0",
            drifted.description(),
            drifted.input().clone(),
            drifted.output().clone(),
            drifted.concurrency().clone(),
            drifted.effect(),
        )?;
        let mut builder = ToolRegistryBuilder::default();
        builder.register(ToolRegistration::new(
            RegisteredToolId::new("stable")?,
            ToolExecutorId::new("executor")?,
            binding("executor")?,
            first.clone(),
            executor.clone(),
        ))?;
        let drift = builder.register(ToolRegistration::new(
            RegisteredToolId::new("stable")?,
            ToolExecutorId::new("executor")?,
            binding("executor")?,
            drifted,
            executor,
        ));
        assert_eq!(
            drift.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );
        let frozen = builder.freeze();
        assert_eq!(frozen.definitions()[0].name().as_str(), "first");
        assert_eq!(frozen.definitions()[0], first);
        Ok(())
    }
}
