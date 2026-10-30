use super::{
    Arc, CallId, Deserialize, Deserializer, Digest, EventToolProgress, RegisteredToolId, Serialize,
    Serializer, Timestamp, ToolAuditIdentity, ToolAuditIdentityWire, ToolCallAttemptIdentity,
    ToolCallObservation, ToolCallerClass, ToolCallerLineage, ToolCallerLineageView, ToolExecutorId,
    ToolExposureBinding, ToolInvocation, ToolName, ToolProgress, ToolProgressSummary,
    ToolResultObservation, fmt,
};

impl ToolAuditIdentity {
    /// Captures immutable audit evidence from an identity-bound, validated invocation.
    #[must_use]
    pub fn from_invocation(invocation: &ToolInvocation) -> Self {
        let registered = invocation.identity();
        let exposure = invocation.exposure();
        Self {
            canonical_name: registered.name().clone(),
            exposed_name: exposure.map(|binding| binding.exposed_name().clone()),
            exposure_map_digest: exposure.map(ToolExposureBinding::map_digest),
            registered_tool_id: registered.tool_id().clone(),
            executor_id: registered.executor_id().clone(),
            definition_digest: registered.definition_digest(),
            schema_digest: registered.schema_digest(),
            effect_metadata_digest: registered.effect_metadata_digest(),
            argument_limits_digest: registered.argument_limits_digest(),
        }
    }

    /// Returns the canonical registered model-facing name.
    #[must_use]
    pub const fn canonical_name(&self) -> &ToolName {
        &self.canonical_name
    }

    /// Returns the provider-facing alias, when an exposure map prepared the invocation.
    #[must_use]
    pub const fn exposed_name(&self) -> Option<&ToolName> {
        self.exposed_name.as_ref()
    }

    /// Returns the exact frozen exposure-map digest, when one selected the invocation.
    #[must_use]
    pub const fn exposure_map_digest(&self) -> Option<Digest> {
        self.exposure_map_digest
    }

    /// Returns the stable application-scoped registered tool identity.
    #[must_use]
    pub const fn registered_tool_id(&self) -> &RegisteredToolId {
        &self.registered_tool_id
    }

    /// Returns the stable executable implementation identity.
    #[must_use]
    pub const fn executor_id(&self) -> &ToolExecutorId {
        &self.executor_id
    }

    /// Returns the complete canonical tool-definition digest.
    #[must_use]
    pub const fn definition_digest(&self) -> Digest {
        self.definition_digest
    }

    /// Returns the canonical parameter-schema digest.
    #[must_use]
    pub const fn schema_digest(&self) -> Digest {
        self.schema_digest
    }

    /// Returns the digest binding the tool effect class and recovery semantics.
    #[must_use]
    pub const fn effect_metadata_digest(&self) -> Digest {
        self.effect_metadata_digest
    }

    /// Returns the digest binding every validated argument-limit field.
    #[must_use]
    pub const fn argument_limits_digest(&self) -> Digest {
        self.argument_limits_digest
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.canonical_name
            .as_str()
            .len()
            .saturating_add(
                self.exposed_name
                    .as_ref()
                    .map_or(0, |name| name.as_str().len()),
            )
            .saturating_add(self.registered_tool_id.as_str().len())
            .saturating_add(self.executor_id.as_str().len())
            .saturating_add(size_of::<Digest>().saturating_mul(
                if self.exposure_map_digest.is_some() {
                    5
                } else {
                    4
                },
            ))
    }
}

impl fmt::Debug for ToolAuditIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolAuditIdentity")
            .field("canonical_name", &self.canonical_name)
            .field("exposed_name", &self.exposed_name)
            .field("exposure_map_digest", &self.exposure_map_digest)
            .field("registered_tool_id", &self.registered_tool_id)
            .field("executor_id", &self.executor_id)
            .field("definition_digest", &self.definition_digest)
            .field("schema_digest", &self.schema_digest)
            .field("effect_metadata_digest", &self.effect_metadata_digest)
            .field("argument_limits_digest", &self.argument_limits_digest)
            .finish()
    }
}

impl Serialize for ToolAuditIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ToolAuditIdentityWire {
            canonical_name: self.canonical_name.clone(),
            exposed_name: self.exposed_name.clone(),
            exposure_map_digest: self.exposure_map_digest,
            registered_tool_id: self.registered_tool_id.as_str().to_owned(),
            executor_id: self.executor_id.as_str().to_owned(),
            definition_digest: self.definition_digest,
            schema_digest: self.schema_digest,
            effect_metadata_digest: self.effect_metadata_digest,
            argument_limits_digest: self.argument_limits_digest,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolAuditIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolAuditIdentityWire::deserialize(deserializer)?;
        Ok(Self {
            canonical_name: wire.canonical_name,
            exposed_name: wire.exposed_name,
            exposure_map_digest: wire.exposure_map_digest,
            registered_tool_id: RegisteredToolId::new(wire.registered_tool_id)
                .map_err(serde::de::Error::custom)?,
            executor_id: ToolExecutorId::new(wire.executor_id).map_err(serde::de::Error::custom)?,
            definition_digest: wire.definition_digest,
            schema_digest: wire.schema_digest,
            effect_metadata_digest: wire.effect_metadata_digest,
            argument_limits_digest: wire.argument_limits_digest,
        })
    }
}

impl ToolCallerLineageView {
    /// Returns the lineage of a call the model issued directly.
    #[must_use]
    pub fn direct() -> Self {
        Self::default()
    }

    /// Projects the canonical caller lineage minted on the child call.
    ///
    /// `parent_call_id` names the outer local-composition marker call that owns
    /// the evaluator cell; it is `None` for a direct call, and `None` for a
    /// provider-hosted program that does not name an owning call.
    #[must_use]
    pub fn from_lineage(lineage: &ToolCallerLineage, parent_call_id: Option<CallId>) -> Self {
        match lineage {
            ToolCallerLineage::ProviderProgrammatic { execution_id, .. } => Self {
                class: ToolCallerClass::ProviderProgrammatic,
                cell_id: Some(Arc::from(execution_id.as_str())),
                parent_call_id,
                nested_ordinal: None,
                depth: None,
            },
            ToolCallerLineage::LocalProgrammatic {
                cell_id,
                request_sequence,
                depth,
                ..
            } => Self {
                class: ToolCallerClass::LocalProgrammatic,
                cell_id: Some(Arc::from(cell_id.as_str())),
                parent_call_id,
                nested_ordinal: Some(*request_sequence),
                depth: Some(*depth),
            },
            ToolCallerLineage::Direct => Self::direct(),
        }
    }

    /// Returns the lineage of an outer local-composition marker call.
    ///
    /// The marker owns a cell rather than belonging to one, so it carries no
    /// parent, no ordinal, and no depth. `cell_id` is the driver's active cell
    /// when the interpreter has already minted one.
    #[must_use]
    pub fn local_composition_marker(cell_id: Option<&str>) -> Self {
        Self {
            class: ToolCallerClass::LocalProgrammatic,
            cell_id: cell_id.map(Arc::from),
            parent_call_id: None,
            nested_ordinal: None,
            depth: None,
        }
    }

    /// Returns the closed caller class.
    #[must_use]
    pub const fn class(&self) -> ToolCallerClass {
        self.class
    }

    /// Returns the evaluator cell, or the hosted execution identity.
    #[must_use]
    pub fn cell_id(&self) -> Option<&str> {
        self.cell_id.as_deref()
    }

    /// Returns the outer marker call that owns the cell, when nested.
    #[must_use]
    pub const fn parent_call_id(&self) -> Option<CallId> {
        self.parent_call_id
    }

    /// Returns the one-based acceptance sequence within the owning cell.
    #[must_use]
    pub const fn nested_ordinal(&self) -> Option<u32> {
        self.nested_ordinal
    }

    /// Returns the independent local-composition recursion depth.
    #[must_use]
    pub const fn depth(&self) -> Option<u16> {
        self.depth
    }

    /// Returns `true` when the call is the outer marker owning an evaluator cell.
    #[must_use]
    pub const fn is_composition_marker(&self) -> bool {
        matches!(self.class, ToolCallerClass::LocalProgrammatic) && self.parent_call_id.is_none()
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.cell_id.as_deref().map_or(0, str::len)
    }
}

impl ToolCallObservation {
    pub(crate) fn new(subject: Option<String>, arguments_preview: String) -> Self {
        Self {
            subject: subject.map(Arc::from),
            arguments_preview: Arc::from(arguments_preview),
        }
    }

    /// Returns the compact redacted subject, when one could be derived.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// Returns bounded redacted arguments suitable for presentation.
    #[must_use]
    pub fn arguments_preview(&self) -> &str {
        &self.arguments_preview
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.subject.as_ref().map_or(0, |value| value.len()) + self.arguments_preview.len()
    }
}

impl ToolResultObservation {
    pub(crate) fn new(succeeded: bool, preview: String, subject: Option<String>) -> Self {
        Self {
            succeeded,
            preview: Arc::from(preview),
            subject: subject.map(Arc::from),
        }
    }

    /// Whether the canonical model-visible result reports success.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// Returns the bounded redacted result preview.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Returns the tool-provided presentation subject, when this result has one.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.preview
            .len()
            .saturating_add(self.subject.as_ref().map_or(0, |value| value.len()))
    }
}

impl ToolCallAttemptIdentity {
    pub(crate) const fn new(
        model_facing_name: ToolName,
        exposure_map_digest: Option<Digest>,
    ) -> Self {
        Self {
            model_facing_name,
            exposure_map_digest,
        }
    }

    /// Returns the bounded name submitted by the model without asserting local identity.
    #[must_use]
    pub const fn model_facing_name(&self) -> &ToolName {
        &self.model_facing_name
    }

    /// Returns the exact active exposure-map digest, when calls used an exposed namespace.
    #[must_use]
    pub const fn exposure_map_digest(&self) -> Option<Digest> {
        self.exposure_map_digest
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.model_facing_name
            .as_str()
            .len()
            .saturating_add(self.exposure_map_digest.map_or(0, |_| size_of::<Digest>()))
    }
}

impl ToolProgressSummary {
    /// Creates exact ordered content-free progress counters.
    #[must_use]
    pub const fn new(
        sequence: u64,
        timestamp: Timestamp,
        completed: Option<u64>,
        total: Option<u64>,
    ) -> Self {
        Self {
            sequence,
            timestamp,
            completed,
            total,
        }
    }

    /// Captures the safe counters from one bounded progress update.
    #[must_use]
    pub fn from_progress(progress: &ToolProgress) -> Self {
        Self {
            sequence: progress.sequence(),
            timestamp: progress.timestamp(),
            completed: progress.completed(),
            total: progress.total(),
        }
    }

    pub(in crate::event) fn from_event_progress(progress: &EventToolProgress) -> Self {
        Self {
            sequence: progress.sequence(),
            timestamp: progress.timestamp(),
            completed: progress.completed(),
            total: progress.total(),
        }
    }

    /// Returns the monotonically increasing per-call sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Returns the injected event timestamp.
    #[must_use]
    pub const fn timestamp(self) -> Timestamp {
        self.timestamp
    }

    /// Returns completed units when reported.
    #[must_use]
    pub const fn completed(self) -> Option<u64> {
        self.completed
    }

    /// Returns total units when reported.
    #[must_use]
    pub const fn total(self) -> Option<u64> {
        self.total
    }
}

impl EventToolProgress {
    /// Returns the monotonically increasing per-call sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the correlated canonical call identity.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the injected event timestamp.
    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns bounded model-independent progress text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns completed units when reported.
    #[must_use]
    pub const fn completed(&self) -> Option<u64> {
        self.completed
    }

    /// Returns total units when reported.
    #[must_use]
    pub const fn total(&self) -> Option<u64> {
        self.total
    }
}

impl From<&ToolProgress> for EventToolProgress {
    fn from(progress: &ToolProgress) -> Self {
        Self {
            sequence: progress.sequence(),
            call_id: progress.call_id(),
            timestamp: progress.timestamp(),
            message: progress.message().to_owned(),
            completed: progress.completed(),
            total: progress.total(),
        }
    }
}

impl PartialEq<ToolProgress> for EventToolProgress {
    fn eq(&self, other: &ToolProgress) -> bool {
        self.sequence == other.sequence()
            && self.call_id == other.call_id()
            && self.timestamp == other.timestamp()
            && self.message == other.message()
            && self.completed == other.completed()
            && self.total == other.total()
    }
}
