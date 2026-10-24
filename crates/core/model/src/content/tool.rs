use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{CallId, McpServerId, NamespaceId, ToolName};

use super::{
    AudioBlock, ContentError, DocumentBlock, ImageBlock, MAX_CANONICAL_BLOCK_BYTES,
    MAX_CORRELATION_ID_BYTES, MAX_TOOL_RESULT_PARTS, OpaqueBlock, TextBlock, ensure_maximum,
    ensure_nonempty,
};

/// The runtime owner of a tool call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolExecutionOwner {
    /// A tool executed by the local Plakous runtime.
    Local,
    /// A tool executed through a configured remote tool server.
    Mcp {
        /// The server that owns execution.
        server: McpServerId,
    },
    /// A tool executed by the remote inference service.
    ProviderHosted {
        /// A stable service-scoped tool kind.
        provider_kind: NamespaceId,
    },
}

/// The lifecycle state of a canonical tool call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolCallState {
    /// The call was requested but has not been accepted for execution.
    Requested,
    /// Execution is pending remotely or locally.
    Pending,
    /// Execution completed successfully.
    Completed,
    /// Execution completed with a model-visible failure.
    Failed,
}

/// Closed caller lineage retained independently from execution ownership.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallerLineage {
    /// The model called the configured tool directly.
    #[default]
    Direct,
    /// A provider-hosted program called the configured tool.
    ProviderProgrammatic {
        /// Stable provider-neutral hosted-program kind.
        kind: NamespaceId,
        /// Exact provider execution identity that emitted the nested call.
        execution_id: String,
    },
    /// A Plakous-local evaluator called a configured tool through the private driver.
    LocalProgrammatic {
        /// Durable operation identity of the one owning evaluation.
        evaluation_operation_id: u64,
        /// Exact interpreter generation that emitted the nested call.
        interpreter_generation: u64,
        /// Exact evaluator cell identity that emitted the nested call.
        cell_id: String,
        /// Driver-assigned acceptance sequence within that evaluation.
        request_sequence: u32,
        /// Canonical child tool-call identity derived from the frozen edge.
        child_call_id: CallId,
        /// Independent local-composition recursion depth.
        depth: u16,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ToolCallerLineageWire {
    Direct,
    ProviderProgrammatic {
        kind: NamespaceId,
        execution_id: String,
    },
    LocalProgrammatic {
        evaluation_operation_id: u64,
        interpreter_generation: u64,
        cell_id: String,
        request_sequence: u32,
        child_call_id: CallId,
        depth: u16,
    },
}

impl<'de> Deserialize<'de> for ToolCallerLineage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ToolCallerLineageWire::deserialize(deserializer)? {
            ToolCallerLineageWire::Direct => Ok(Self::Direct),
            ToolCallerLineageWire::ProviderProgrammatic { kind, execution_id } => {
                Self::provider_programmatic(kind, execution_id).map_err(serde::de::Error::custom)
            }
            ToolCallerLineageWire::LocalProgrammatic {
                evaluation_operation_id,
                interpreter_generation,
                cell_id,
                request_sequence,
                child_call_id,
                depth,
            } => Self::local_programmatic(
                evaluation_operation_id,
                interpreter_generation,
                cell_id,
                request_sequence,
                child_call_id,
                depth,
            )
            .map_err(serde::de::Error::custom),
        }
    }
}

impl ToolCallerLineage {
    /// Creates validated local-programmatic caller lineage.
    ///
    /// # Errors
    ///
    /// Returns an error when an owning operation, interpreter generation, request
    /// sequence, or depth is zero, or when the cell identity is empty or oversized.
    pub fn local_programmatic(
        evaluation_operation_id: u64,
        interpreter_generation: u64,
        cell_id: impl Into<String>,
        request_sequence: u32,
        child_call_id: CallId,
        depth: u16,
    ) -> Result<Self, ContentError> {
        if evaluation_operation_id == 0 {
            return Err(ContentError::Empty {
                field: "local programmatic evaluation operation ID",
            });
        }
        if interpreter_generation == 0 {
            return Err(ContentError::Empty {
                field: "local programmatic interpreter generation",
            });
        }
        let cell_id = cell_id.into();
        ensure_nonempty(&cell_id, "local programmatic cell ID")?;
        ensure_maximum(
            cell_id.len(),
            "local programmatic cell ID",
            MAX_CORRELATION_ID_BYTES,
        )?;
        if request_sequence == 0 {
            return Err(ContentError::Empty {
                field: "local programmatic request sequence",
            });
        }
        if depth == 0 {
            return Err(ContentError::Empty {
                field: "local programmatic depth",
            });
        }
        Ok(Self::LocalProgrammatic {
            evaluation_operation_id,
            interpreter_generation,
            cell_id,
            request_sequence,
            child_call_id,
            depth,
        })
    }

    /// Creates validated provider-programmatic caller lineage.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider execution identity is empty or oversized.
    pub fn provider_programmatic(
        kind: NamespaceId,
        execution_id: impl Into<String>,
    ) -> Result<Self, ContentError> {
        let execution_id = execution_id.into();
        ensure_nonempty(&execution_id, "provider programmatic execution ID")?;
        ensure_maximum(
            execution_id.len(),
            "provider programmatic execution ID",
            MAX_CORRELATION_ID_BYTES,
        )?;
        Ok(Self::ProviderProgrammatic { kind, execution_id })
    }

    /// Returns the hosted-program kind for provider-programmatic lineage.
    #[must_use]
    pub const fn provider_programmatic_kind(&self) -> Option<&NamespaceId> {
        match self {
            Self::Direct | Self::LocalProgrammatic { .. } => None,
            Self::ProviderProgrammatic { kind, .. } => Some(kind),
        }
    }

    /// Returns the hosted execution identity for provider-programmatic lineage.
    #[must_use]
    pub fn provider_programmatic_execution_id(&self) -> Option<&str> {
        match self {
            Self::Direct | Self::LocalProgrammatic { .. } => None,
            Self::ProviderProgrammatic { execution_id, .. } => Some(execution_id),
        }
    }
}

/// Remote correlation, container, and caller lineage retained for replay.
#[allow(
    clippy::struct_field_names,
    reason = "the ID suffix distinguishes all canonical correlation identities"
)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ToolCorrelation {
    remote_call_id: Option<String>,
    container_id: Option<String>,
    caller_lineage: ToolCallerLineage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "the correlation wire field names are part of the serialized vocabulary"
)]
struct ToolCorrelationWire {
    remote_call_id: Option<String>,
    container_id: Option<String>,
    caller_lineage: ToolCallerLineage,
}

impl<'de> Deserialize<'de> for ToolCorrelation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolCorrelationWire::deserialize(deserializer)?;
        Self::new(wire.remote_call_id, wire.container_id, wire.caller_lineage)
            .map_err(serde::de::Error::custom)
    }
}

impl ToolCorrelation {
    /// Creates validated optional correlation values.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when a present correlation value is empty.
    pub fn new(
        remote_call_id: Option<String>,
        container_id: Option<String>,
        caller_lineage: ToolCallerLineage,
    ) -> Result<Self, ContentError> {
        for (value, field) in [
            (remote_call_id.as_deref(), "remote call ID"),
            (container_id.as_deref(), "container ID"),
        ] {
            if value.is_some_and(str::is_empty) {
                return Err(ContentError::Empty { field });
            }
            if let Some(value) = value {
                ensure_maximum(value.len(), field, MAX_CORRELATION_ID_BYTES)?;
            }
        }
        Ok(Self {
            remote_call_id,
            container_id,
            caller_lineage,
        })
    }

    /// Returns the remote correlation ID.
    #[must_use]
    pub fn remote_call_id(&self) -> Option<&str> {
        self.remote_call_id.as_deref()
    }

    /// Returns the remote container ID.
    #[must_use]
    pub fn container_id(&self) -> Option<&str> {
        self.container_id.as_deref()
    }

    /// Returns caller lineage independently from execution ownership.
    #[must_use]
    pub const fn caller_lineage(&self) -> &ToolCallerLineage {
        &self.caller_lineage
    }
}

/// A canonical tool call retaining raw and parsed arguments.
///
/// It records both names of the call: the canonical executable name and the
/// exact name the model called. A call built from one emitted name records it
/// as both until the runtime resolves the canonical name at landing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolCallBlock {
    call_id: CallId,
    canonical_name: ToolName,
    exposed_name: ToolName,
    owner: ToolExecutionOwner,
    state: ToolCallState,
    correlation: ToolCorrelation,
    raw_arguments: String,
    arguments: Option<Value>,
}

#[derive(Deserialize)]
struct ToolCallBlockWire {
    call_id: CallId,
    canonical_name: ToolName,
    exposed_name: ToolName,
    owner: ToolExecutionOwner,
    state: ToolCallState,
    correlation: ToolCorrelation,
    raw_arguments: String,
    arguments: Option<Value>,
}

impl<'de> Deserialize<'de> for ToolCallBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolCallBlockWire::deserialize(deserializer)?;
        let block = Self::new(
            wire.call_id,
            wire.exposed_name,
            wire.owner,
            wire.state,
            wire.correlation,
            wire.raw_arguments,
        )
        .map_err(serde::de::Error::custom)?
        .with_canonical_name(wire.canonical_name);
        // History is stored as JCS: e.g. 1.0 becomes 1, and large integers
        // round to binary64. Validate in that same domain, but retain the
        // value parsed from the exact raw text for in-memory readers.
        let arguments_match = match (&block.arguments, &wire.arguments) {
            (Some(parsed), Some(stored)) => {
                parsed == stored
                    || serde_json_canonicalizer::to_vec(parsed).map_err(serde::de::Error::custom)?
                        == serde_json_canonicalizer::to_vec(stored)
                            .map_err(serde::de::Error::custom)?
            }
            // Option<Value> decodes a serialized Some(Null) as None.
            (Some(Value::Null) | None, None) => true,
            _ => false,
        };
        if !arguments_match {
            return Err(serde::de::Error::custom(
                ContentError::ToolArgumentsMismatch,
            ));
        }
        Ok(block)
    }
}

impl ToolCallBlock {
    /// Creates a tool call and parses its raw JSON arguments when valid.
    ///
    /// `tool` is the name the model called; it is also the canonical name
    /// until [`Self::with_canonical_name`] records the resolved one.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when raw arguments are empty. Malformed
    /// non-empty JSON remains available through [`Self::raw_arguments`].
    pub fn new(
        call_id: CallId,
        tool: ToolName,
        owner: ToolExecutionOwner,
        state: ToolCallState,
        correlation: ToolCorrelation,
        raw_arguments: impl Into<String>,
    ) -> Result<Self, ContentError> {
        let raw_arguments = raw_arguments.into();
        ensure_nonempty(&raw_arguments, "tool arguments")?;
        ensure_maximum(
            raw_arguments.len(),
            "tool arguments",
            MAX_CANONICAL_BLOCK_BYTES,
        )?;
        let arguments = serde_json::from_str(&raw_arguments).ok();
        Ok(Self {
            call_id,
            canonical_name: tool.clone(),
            exposed_name: tool,
            owner,
            state,
            correlation,
            raw_arguments,
            arguments,
        })
    }

    /// Returns the canonical call ID.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Records the canonical executable name the exposed name resolved to.
    #[must_use]
    pub fn with_canonical_name(mut self, canonical_name: ToolName) -> Self {
        self.canonical_name = canonical_name;
        self
    }

    /// Returns the canonical executable name: approval identity, topology
    /// detection, plan restore and classification bind it.
    #[must_use]
    pub const fn canonical_name(&self) -> &ToolName {
        &self.canonical_name
    }

    /// Returns the exact name the model called, in the namespace it called it
    /// from: provider replay sends it, and views label the call by it.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Returns the execution owner.
    #[must_use]
    pub const fn owner(&self) -> &ToolExecutionOwner {
        &self.owner
    }

    /// Returns the lifecycle state.
    #[must_use]
    pub const fn state(&self) -> ToolCallState {
        self.state
    }

    /// Returns replay correlation values.
    #[must_use]
    pub const fn correlation(&self) -> &ToolCorrelation {
        &self.correlation
    }

    /// Returns exact assembled argument text.
    #[must_use]
    pub fn raw_arguments(&self) -> &str {
        &self.raw_arguments
    }

    /// Returns parsed arguments after successful JSON validation.
    #[must_use]
    pub const fn arguments(&self) -> Option<&Value> {
        self.arguments.as_ref()
    }

    /// Records the canonical name and owner the runtime resolved.
    pub(super) fn resolve_runtime(&mut self, canonical_name: ToolName, owner: ToolExecutionOwner) {
        self.canonical_name = canonical_name;
        self.owner = owner;
    }
}

/// An ordered model-visible tool result part.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolResultPart {
    /// Text result content.
    Text(TextBlock),
    /// Image result content.
    Image(ImageBlock),
    /// Document result content.
    Document(DocumentBlock),
    /// Audio result content.
    Audio(AudioBlock),
    /// Opaque scoped result content.
    Opaque(OpaqueBlock),
}

/// A nonterminal, model-visible update associated with an existing tool call.
///
/// Updates are distinct from [`ToolResultBlock`]: any number may be retained,
/// while exactly one terminal result continues to satisfy the call.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolResultUpdateBlock {
    call_id: CallId,
    owner: ToolExecutionOwner,
    text: TextBlock,
    correlation: ToolCorrelation,
}

#[derive(Deserialize)]
struct ToolResultUpdateBlockWire {
    call_id: CallId,
    owner: ToolExecutionOwner,
    text: TextBlock,
    correlation: ToolCorrelation,
}

impl<'de> Deserialize<'de> for ToolResultUpdateBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolResultUpdateBlockWire::deserialize(deserializer)?;
        Self::new(wire.call_id, wire.owner, wire.text, wire.correlation)
            .map_err(serde::de::Error::custom)
    }
}

impl ToolResultUpdateBlock {
    /// Creates one bounded text update for an existing local tool call.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner is not locally executed.
    pub fn new(
        call_id: CallId,
        owner: ToolExecutionOwner,
        text: TextBlock,
        correlation: ToolCorrelation,
    ) -> Result<Self, ContentError> {
        if !matches!(
            owner,
            ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
        ) {
            return Err(ContentError::InvalidToolResultUpdateOwner);
        }
        Ok(Self {
            call_id,
            owner,
            text,
            correlation,
        })
    }

    #[must_use]
    /// Returns the canonical call ID.
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }
    #[must_use]
    /// Returns the execution owner.
    pub const fn owner(&self) -> &ToolExecutionOwner {
        &self.owner
    }
    #[must_use]
    /// Returns the update text.
    pub const fn text(&self) -> &TextBlock {
        &self.text
    }
    #[must_use]
    /// Returns replay correlation values.
    pub const fn correlation(&self) -> &ToolCorrelation {
        &self.correlation
    }
}

impl ToolResultPart {
    pub(super) fn retained_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.text().len(),
            Self::Image(image) => image.media_type().len() + image.source().retained_bytes(),
            Self::Document(document) => {
                document.media_type().len() + document.source().retained_bytes()
            }
            Self::Audio(audio) => audio.media_type().len() + audio.source().retained_bytes(),
            Self::Opaque(opaque) => opaque.retained_bytes(),
        }
    }
}

/// The semantic outcome kind of a tool result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolResultKind {
    /// Model-visible success.
    Success,
    /// Model-visible failure.
    Failure,
}

/// A canonical tool result correlated to a stable call ID.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolResultBlock {
    call_id: CallId,
    owner: ToolExecutionOwner,
    kind: ToolResultKind,
    parts: Vec<ToolResultPart>,
    correlation: ToolCorrelation,
}

#[derive(Deserialize)]
struct ToolResultBlockWire {
    call_id: CallId,
    owner: ToolExecutionOwner,
    kind: ToolResultKind,
    parts: Vec<ToolResultPart>,
    correlation: ToolCorrelation,
}

impl<'de> Deserialize<'de> for ToolResultBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolResultBlockWire::deserialize(deserializer)?;
        Self::new(
            wire.call_id,
            wire.owner,
            wire.kind,
            wire.parts,
            wire.correlation,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl ToolResultBlock {
    /// Creates a non-empty ordered tool result.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::EmptyToolResult`] when `parts` is empty.
    pub fn new(
        call_id: CallId,
        owner: ToolExecutionOwner,
        kind: ToolResultKind,
        parts: Vec<ToolResultPart>,
        correlation: ToolCorrelation,
    ) -> Result<Self, ContentError> {
        if parts.is_empty() {
            return Err(ContentError::EmptyToolResult);
        }
        ensure_maximum(parts.len(), "tool result parts", MAX_TOOL_RESULT_PARTS)?;
        let retained_bytes = parts.iter().fold(0_usize, |total, part| {
            total.saturating_add(part.retained_bytes())
        });
        ensure_maximum(
            retained_bytes,
            "tool result content",
            MAX_CANONICAL_BLOCK_BYTES,
        )?;
        Ok(Self {
            call_id,
            owner,
            kind,
            parts,
            correlation,
        })
    }

    /// Returns the canonical call ID.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the execution owner.
    #[must_use]
    pub const fn owner(&self) -> &ToolExecutionOwner {
        &self.owner
    }

    /// Returns the result kind.
    #[must_use]
    pub const fn kind(&self) -> ToolResultKind {
        self.kind
    }

    /// Returns ordered multimodal result parts.
    #[must_use]
    pub fn parts(&self) -> &[ToolResultPart] {
        &self.parts
    }

    /// Returns replay correlation values.
    #[must_use]
    pub const fn correlation(&self) -> &ToolCorrelation {
        &self.correlation
    }
}
