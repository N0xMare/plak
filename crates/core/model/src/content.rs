//! Ordered canonical conversation, content, and replay values.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, value::RawValue};
use thiserror::Error;

use crate::{
    CallId, Digest, DigestDomain, DigestError, EntryId, McpServerId, ModelFamilyId, ModelId,
    NamespaceId, ProtocolId, ReplayKindId, ToolName,
};

const MAX_CANONICAL_BLOCK_BYTES: usize = 16 * 1024 * 1024;
const MAX_MEDIA_URI_BYTES: usize = 8 * 1024;
const MAX_MEDIA_TYPE_BYTES: usize = 256;
const MAX_CORRELATION_ID_BYTES: usize = 4 * 1024;
const MAX_TOOL_RESULT_PARTS: usize = 1_024;
const MAX_CONVERSATION_BLOCKS: usize = 4_096;

/// An error constructing canonical conversation content.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContentError {
    /// A required string or byte sequence was empty.
    #[error("{field} cannot be empty")]
    Empty {
        /// The semantic field that was empty.
        field: &'static str,
    },
    /// A field exceeded its canonical in-memory byte or element budget.
    #[error("{field} exceeds the canonical limit of {maximum}")]
    TooLarge {
        /// The semantic field that exceeded its limit.
        field: &'static str,
        /// The maximum accepted bytes or elements.
        maximum: usize,
    },
    /// A replay envelope used reserved version zero.
    #[error("replay envelope version must be nonzero")]
    ZeroReplayVersion,
    /// A replay envelope's declared digest did not match its identity and payload.
    #[error("replay envelope digest mismatch")]
    ReplayDigestMismatch,
    /// A digest could not be constructed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// A reasoning visibility state did not match its content.
    #[error("reasoning content does not match its visibility state")]
    InvalidReasoning,
    /// A citation range ended before it started.
    #[error("citation range end must not precede its start")]
    InvalidCitationRange,
    /// A tool result had no ordered result parts.
    #[error("tool result must contain at least one part")]
    EmptyToolResult,
    /// A data URL source did not use the required `data:` scheme.
    #[error("media data URL must begin with data:")]
    InvalidDataUrl,
    /// A nonterminal tool-result update used a provider-hosted owner.
    #[error("tool result updates require a locally executed owner")]
    InvalidToolResultUpdateOwner,
    /// Serialized parsed tool arguments disagreed with their exact raw form.
    #[error("parsed tool arguments do not match exact raw arguments")]
    ToolArgumentsMismatch,
    /// A conversation item had no blocks.
    #[error("conversation item must contain at least one block")]
    EmptyConversationItem,
}

/// The actor responsible for a canonical conversation item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum Actor {
    /// Developer-authored instruction or history.
    Developer,
    /// System-authored instruction or history.
    System,
    /// User-authored input.
    User,
    /// Model-authored output.
    Assistant,
    /// Tool-authored output.
    Tool,
    /// Harness-authored context.
    Harness,
}

impl Actor {
    /// Stable lowercase actor name for readable history projections.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Developer => "developer",
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Harness => "harness",
        }
    }
}

/// The origin of a conversation item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum Provenance {
    /// Direct user input.
    UserInput,
    /// Model output assembled from a model operation.
    ModelOutput,
    /// A tool execution result.
    ToolExecution,
    /// Active or historical instruction content.
    Instruction,
    /// Imported history whose original source is external.
    Imported,
}

/// A semantic phase for text output.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum TextPhase {
    /// Intermediate commentary intended for display.
    Commentary,
    /// The final answer projection.
    Final,
}

/// A validated text content block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TextBlock {
    text: String,
    phase: Option<TextPhase>,
}

#[derive(Deserialize)]
struct TextBlockWire {
    text: String,
    phase: Option<TextPhase>,
}

impl<'de> Deserialize<'de> for TextBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TextBlockWire::deserialize(deserializer)?;
        Self::new(wire.text, wire.phase).map_err(serde::de::Error::custom)
    }
}

impl TextBlock {
    /// Creates a non-empty text block.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when `text` is empty.
    pub fn new(text: impl Into<String>, phase: Option<TextPhase>) -> Result<Self, ContentError> {
        let text = text.into();
        ensure_nonempty(&text, "text")?;
        ensure_maximum(text.len(), "text", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self { text, phase })
    }

    /// Returns the text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the optional semantic phase.
    #[must_use]
    pub const fn phase(&self) -> Option<TextPhase> {
        self.phase
    }
}

/// The visibility and replay policy of reasoning content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ReasoningVisibility {
    /// A displayable semantic summary.
    Summary,
    /// Visible reasoning explicitly authorized for display.
    Visible,
    /// Reasoning that was omitted by the source.
    Omitted,
    /// Reasoning whose content was redacted.
    Redacted,
    /// Opaque replay-only reasoning material.
    OpaqueReplay,
}

/// A canonical reasoning block without hidden chain-of-thought interpretation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReasoningBlock {
    visibility: ReasoningVisibility,
    text: Option<String>,
    replay: Option<ReplayEnvelope>,
}

#[derive(Deserialize)]
struct ReasoningBlockWire {
    visibility: ReasoningVisibility,
    text: Option<String>,
    replay: Option<ReplayEnvelope>,
}

impl<'de> Deserialize<'de> for ReasoningBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReasoningBlockWire::deserialize(deserializer)?;
        Self::new(wire.visibility, wire.text, wire.replay).map_err(serde::de::Error::custom)
    }
}

impl ReasoningBlock {
    /// Creates a validated reasoning block.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::InvalidReasoning`] when visible states lack text,
    /// hidden states contain text, or replay-only state lacks an envelope.
    pub fn new(
        visibility: ReasoningVisibility,
        text: Option<String>,
        replay: Option<ReplayEnvelope>,
    ) -> Result<Self, ContentError> {
        let valid = match visibility {
            ReasoningVisibility::Summary | ReasoningVisibility::Visible => {
                text.as_deref().is_some_and(|value| !value.is_empty())
            }
            ReasoningVisibility::Omitted | ReasoningVisibility::Redacted => text.is_none(),
            ReasoningVisibility::OpaqueReplay => text.is_none() && replay.is_some(),
        };
        if !valid {
            return Err(ContentError::InvalidReasoning);
        }
        if let Some(text) = &text {
            ensure_maximum(text.len(), "reasoning text", MAX_CANONICAL_BLOCK_BYTES)?;
        }
        Ok(Self {
            visibility,
            text,
            replay,
        })
    }

    /// Returns the visibility state.
    #[must_use]
    pub const fn visibility(&self) -> ReasoningVisibility {
        self.visibility
    }

    /// Returns displayable text when present.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Returns opaque replay material when present.
    #[must_use]
    pub const fn replay(&self) -> Option<&ReplayEnvelope> {
        self.replay.as_ref()
    }
}

/// An owned media source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MediaSource {
    kind: MediaSourceKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum MediaSourceKind {
    Uri(String),
    DataUrl(String),
    Bytes(Vec<u8>),
}

#[derive(Deserialize)]
struct MediaSourceWire {
    kind: MediaSourceKind,
}

impl<'de> Deserialize<'de> for MediaSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MediaSourceWire::deserialize(deserializer)?;
        match wire.kind {
            MediaSourceKind::Uri(uri) => Self::uri(uri),
            MediaSourceKind::DataUrl(data_url) => Self::data_url(data_url),
            MediaSourceKind::Bytes(bytes) => Self::bytes(bytes),
        }
        .map_err(serde::de::Error::custom)
    }
}

impl MediaSource {
    /// Creates a non-empty URI media source.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when the URI is empty.
    pub fn uri(uri: impl Into<String>) -> Result<Self, ContentError> {
        let uri = uri.into();
        ensure_nonempty(&uri, "media URI")?;
        ensure_maximum(uri.len(), "media URI", MAX_MEDIA_URI_BYTES)?;
        Ok(Self {
            kind: MediaSourceKind::Uri(uri),
        })
    }

    /// Creates an exact, bounded data URL media source.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not a non-empty `data:` URL or is oversized.
    pub fn data_url(data_url: impl Into<String>) -> Result<Self, ContentError> {
        let data_url = data_url.into();
        if !data_url.starts_with("data:") {
            return Err(ContentError::InvalidDataUrl);
        }
        ensure_maximum(data_url.len(), "media data URL", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self {
            kind: MediaSourceKind::DataUrl(data_url),
        })
    }

    /// Creates a non-empty inline byte source.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when no bytes are supplied.
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, ContentError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(ContentError::Empty {
                field: "media bytes",
            });
        }
        ensure_maximum(bytes.len(), "media bytes", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self {
            kind: MediaSourceKind::Bytes(bytes),
        })
    }

    /// Returns the URI when this source references one.
    #[must_use]
    pub fn as_uri(&self) -> Option<&str> {
        match &self.kind {
            MediaSourceKind::Uri(uri) => Some(uri),
            MediaSourceKind::DataUrl(_) | MediaSourceKind::Bytes(_) => None,
        }
    }

    /// Returns the exact data URL when this source retains one.
    #[must_use]
    pub fn as_data_url(&self) -> Option<&str> {
        match &self.kind {
            MediaSourceKind::DataUrl(data_url) => Some(data_url),
            MediaSourceKind::Uri(_) | MediaSourceKind::Bytes(_) => None,
        }
    }

    /// Returns bytes when this source is inline.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.kind {
            MediaSourceKind::Bytes(bytes) => Some(bytes),
            MediaSourceKind::Uri(_) | MediaSourceKind::DataUrl(_) => None,
        }
    }

    fn retained_bytes(&self) -> usize {
        match &self.kind {
            MediaSourceKind::Uri(uri) => uri.len(),
            MediaSourceKind::DataUrl(data_url) => data_url.len(),
            MediaSourceKind::Bytes(bytes) => bytes.len(),
        }
    }
}

macro_rules! media_block {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
        pub struct $name {
            media_type: String,
            source: MediaSource,
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                #[derive(Deserialize)]
                struct Wire {
                    media_type: String,
                    source: MediaSource,
                }

                let wire = Wire::deserialize(deserializer)?;
                Self::new(wire.media_type, wire.source).map_err(serde::de::Error::custom)
            }
        }

        impl $name {
            /// Creates a media block with a non-empty media type.
            ///
            /// # Errors
            ///
            /// Returns [`ContentError::Empty`] when the media type is empty.
            pub fn new(
                media_type: impl Into<String>,
                source: MediaSource,
            ) -> Result<Self, ContentError> {
                let media_type = media_type.into();
                ensure_nonempty(&media_type, "media type")?;
                ensure_maximum(media_type.len(), "media type", MAX_MEDIA_TYPE_BYTES)?;
                Ok(Self { media_type, source })
            }

            /// Returns the declared media type.
            #[must_use]
            pub fn media_type(&self) -> &str {
                &self.media_type
            }

            /// Returns the media source.
            #[must_use]
            pub const fn source(&self) -> &MediaSource {
                &self.source
            }
        }
    };
}

/// Provider-neutral image rendering detail retained for exact family lowering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ImageDetailHint {
    /// Let the target choose detail.
    Auto,
    /// Prefer low detail.
    Low,
    /// Prefer high detail.
    High,
    /// Preserve original detail.
    Original,
}

/// A canonical image block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImageBlock {
    media_type: String,
    source: MediaSource,
    detail: Option<ImageDetailHint>,
}

#[derive(Deserialize)]
struct ImageBlockWire {
    media_type: String,
    source: MediaSource,
    #[serde(default)]
    detail: Option<ImageDetailHint>,
}

impl<'de> Deserialize<'de> for ImageBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ImageBlockWire::deserialize(deserializer)?;
        Self::with_detail(wire.media_type, wire.source, wire.detail)
            .map_err(serde::de::Error::custom)
    }
}

impl ImageBlock {
    /// Creates an image without an explicit rendering detail.
    ///
    /// # Errors
    ///
    /// Returns an error when the media type is empty or oversized.
    pub fn new(media_type: impl Into<String>, source: MediaSource) -> Result<Self, ContentError> {
        Self::with_detail(media_type, source, None)
    }

    /// Creates an image with an optional rendering detail.
    ///
    /// # Errors
    ///
    /// Returns an error when the media type is empty or oversized.
    pub fn with_detail(
        media_type: impl Into<String>,
        source: MediaSource,
        detail: Option<ImageDetailHint>,
    ) -> Result<Self, ContentError> {
        let media_type = media_type.into();
        ensure_nonempty(&media_type, "media type")?;
        ensure_maximum(media_type.len(), "media type", MAX_MEDIA_TYPE_BYTES)?;
        Ok(Self {
            media_type,
            source,
            detail,
        })
    }

    #[must_use]
    /// Returns the declared media type.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    #[must_use]
    /// Returns the retained media source.
    pub const fn source(&self) -> &MediaSource {
        &self.source
    }

    #[must_use]
    /// Returns the optional rendering detail hint.
    pub const fn detail(&self) -> Option<ImageDetailHint> {
        self.detail
    }
}

media_block!(DocumentBlock, "A canonical document block.");
media_block!(AudioBlock, "A canonical audio block.");

/// The runtime owner of a tool call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolExecutionOwner {
    /// A tool executed by the local Plakous runtime.
    PlakLocal,
    /// A tool executed through a configured remote tool server.
    PlakMcp {
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
        if block.arguments != wire.arguments {
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
            ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. }
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
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.text.len(),
            Self::Image(image) => image.media_type.len() + image.source.retained_bytes(),
            Self::Document(document) => {
                document.media_type.len() + document.source.retained_bytes()
            }
            Self::Audio(audio) => audio.media_type.len() + audio.source.retained_bytes(),
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

/// An explicit refusal block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RefusalBlock {
    text: String,
}

#[derive(Deserialize)]
struct RefusalBlockWire {
    text: String,
}

impl<'de> Deserialize<'de> for RefusalBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RefusalBlockWire::deserialize(deserializer)?;
        Self::new(wire.text).map_err(serde::de::Error::custom)
    }
}

impl RefusalBlock {
    /// Creates a non-empty refusal.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when refusal text is empty.
    pub fn new(text: impl Into<String>) -> Result<Self, ContentError> {
        let text = text.into();
        ensure_nonempty(&text, "refusal")?;
        ensure_maximum(text.len(), "refusal", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self { text })
    }

    /// Returns refusal text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// A unit used to interpret citation offsets.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CitationUnit {
    /// Unicode scalar-value offsets.
    UnicodeScalar,
    /// UTF-8 byte offsets.
    Utf8Byte,
    /// UTF-16 code-unit offsets.
    Utf16CodeUnit,
}

/// An explicit citation span.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CitationRange {
    start: u64,
    end: u64,
    unit: CitationUnit,
}

#[derive(Deserialize)]
struct CitationRangeWire {
    start: u64,
    end: u64,
    unit: CitationUnit,
}

impl<'de> Deserialize<'de> for CitationRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CitationRangeWire::deserialize(deserializer)?;
        Self::new(wire.start, wire.end, wire.unit).map_err(serde::de::Error::custom)
    }
}

impl CitationRange {
    /// Creates a half-open citation range.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::InvalidCitationRange`] when `end` precedes `start`.
    pub fn new(start: u64, end: u64, unit: CitationUnit) -> Result<Self, ContentError> {
        if end < start {
            return Err(ContentError::InvalidCitationRange);
        }
        Ok(Self { start, end, unit })
    }

    /// Returns the inclusive start offset.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Returns the offset unit.
    #[must_use]
    pub const fn unit(self) -> CitationUnit {
        self.unit
    }
}

/// A stable target inside a canonical conversation item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CitationTarget {
    item: EntryId,
    block_index: u32,
}

impl CitationTarget {
    /// Creates a target for one block in an item.
    #[must_use]
    pub const fn new(item: EntryId, block_index: u32) -> Self {
        Self { item, block_index }
    }

    /// Returns the target item.
    #[must_use]
    pub const fn item(self) -> EntryId {
        self.item
    }

    /// Returns the target block index.
    #[must_use]
    pub const fn block_index(self) -> u32 {
        self.block_index
    }
}

/// Citation source identity and presentation metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CitationSource {
    source_id: String,
    annotation_id: String,
    uri: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
struct CitationSourceWire {
    source_id: String,
    annotation_id: String,
    uri: Option<String>,
    title: Option<String>,
}

impl<'de> Deserialize<'de> for CitationSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CitationSourceWire::deserialize(deserializer)?;
        Self::new(wire.source_id, wire.annotation_id, wire.uri, wire.title)
            .map_err(serde::de::Error::custom)
    }
}

impl CitationSource {
    /// Creates citation source metadata with stable non-empty identities.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when a required identity or present
    /// presentation value is empty.
    pub fn new(
        source_id: impl Into<String>,
        annotation_id: impl Into<String>,
        uri: Option<String>,
        title: Option<String>,
    ) -> Result<Self, ContentError> {
        let source_id = source_id.into();
        let annotation_id = annotation_id.into();
        ensure_nonempty(&source_id, "citation source ID")?;
        ensure_nonempty(&annotation_id, "citation annotation ID")?;
        ensure_maximum(
            source_id.len(),
            "citation source ID",
            MAX_CORRELATION_ID_BYTES,
        )?;
        ensure_maximum(
            annotation_id.len(),
            "citation annotation ID",
            MAX_CORRELATION_ID_BYTES,
        )?;
        if uri.as_deref().is_some_and(str::is_empty) {
            return Err(ContentError::Empty {
                field: "citation URI",
            });
        }
        if title.as_deref().is_some_and(str::is_empty) {
            return Err(ContentError::Empty {
                field: "citation title",
            });
        }
        if let Some(uri) = &uri {
            ensure_maximum(uri.len(), "citation URI", MAX_MEDIA_URI_BYTES)?;
        }
        if let Some(title) = &title {
            ensure_maximum(title.len(), "citation title", MAX_CORRELATION_ID_BYTES)?;
        }
        Ok(Self {
            source_id,
            annotation_id,
            uri,
            title,
        })
    }

    /// Returns stable source identity.
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Returns stable annotation identity.
    #[must_use]
    pub fn annotation_id(&self) -> &str {
        &self.annotation_id
    }

    /// Returns the optional source URI.
    #[must_use]
    pub fn uri(&self) -> Option<&str> {
        self.uri.as_deref()
    }

    /// Returns the optional display title.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

/// A citation retaining target association and explicit offset units.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CitationBlock {
    target: CitationTarget,
    source: CitationSource,
    range: Option<CitationRange>,
}

impl CitationBlock {
    /// Creates a citation.
    #[must_use]
    pub const fn new(
        target: CitationTarget,
        source: CitationSource,
        range: Option<CitationRange>,
    ) -> Self {
        Self {
            target,
            source,
            range,
        }
    }

    /// Returns the cited target.
    #[must_use]
    pub const fn target(&self) -> CitationTarget {
        self.target
    }

    /// Returns source metadata.
    #[must_use]
    pub const fn source(&self) -> &CitationSource {
        &self.source
    }

    /// Returns the optional target range.
    #[must_use]
    pub const fn range(&self) -> Option<CitationRange> {
        self.range
    }

    fn retained_bytes(&self) -> usize {
        self.source.source_id.len()
            + self.source.annotation_id.len()
            + self.source.uri.as_deref().map_or(0, str::len)
            + self.source.title.as_deref().map_or(0, str::len)
    }
}

/// The compatibility scope of opaque replay material.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ReplayScope {
    /// Compatible with the same protocol dialect.
    Protocol,
    /// Compatible only with the declared model family.
    ModelFamily,
    /// Compatible only with one exact model identity.
    ExactModel(ModelId),
}

/// The data-sensitivity classification of replay material.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum DataClassification {
    /// Data approved for ordinary display.
    Public,
    /// Data requiring log and telemetry redaction.
    Sensitive,
    /// Secret material requiring the strongest storage policy.
    Secret,
}

/// Sensitive protocol replay material with a verified, scoped digest.
#[derive(Clone, Serialize)]
pub struct ReplayEnvelope {
    protocol: ProtocolId,
    model_family: Option<ModelFamilyId>,
    scope: ReplayScope,
    kind: ReplayKindId,
    version: u32,
    classification: DataClassification,
    digest: Digest,
    payload: Box<RawValue>,
}

impl PartialEq for ReplayEnvelope {
    fn eq(&self, other: &Self) -> bool {
        self.protocol == other.protocol
            && self.model_family == other.model_family
            && self.scope == other.scope
            && self.kind == other.kind
            && self.version == other.version
            && self.classification == other.classification
            && self.digest == other.digest
            && self.payload.get() == other.payload.get()
    }
}

impl ReplayEnvelope {
    /// Constructs an envelope and hashes its exact raw payload and identity.
    ///
    /// # Errors
    ///
    /// Returns an error for version zero, a missing required family scope, or a
    /// digest construction failure.
    pub fn new(
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        scope: ReplayScope,
        kind: ReplayKindId,
        version: u32,
        classification: DataClassification,
        payload: Box<RawValue>,
    ) -> Result<Self, ContentError> {
        if version == 0 {
            return Err(ContentError::ZeroReplayVersion);
        }
        ensure_maximum(
            payload.get().len(),
            "replay payload",
            MAX_CANONICAL_BLOCK_BYTES,
        )?;
        if matches!(scope, ReplayScope::ModelFamily) && model_family.is_none() {
            return Err(ContentError::Empty {
                field: "model family for family-scoped replay",
            });
        }
        let digest = replay_digest(
            &protocol,
            model_family.as_ref(),
            &scope,
            &kind,
            version,
            classification,
            &payload,
        )?;
        Ok(Self {
            protocol,
            model_family,
            scope,
            kind,
            version,
            classification,
            digest,
            payload,
        })
    }

    /// Returns the protocol scope.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the optional model-family scope.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// Returns the scope rule.
    #[must_use]
    pub const fn scope(&self) -> &ReplayScope {
        &self.scope
    }

    /// Returns the replay kind.
    #[must_use]
    pub const fn kind(&self) -> &ReplayKindId {
        &self.kind
    }

    /// Returns the replay format version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Returns the sensitivity classification.
    #[must_use]
    pub const fn classification(&self) -> DataClassification {
        self.classification
    }

    /// Returns the verified replay digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// Returns the exact raw JSON payload.
    #[must_use]
    pub fn payload(&self) -> &RawValue {
        &self.payload
    }

    /// Reports whether the target binding may consume this replay material.
    #[must_use]
    pub fn is_compatible(
        &self,
        protocol: &ProtocolId,
        model_family: Option<&ModelFamilyId>,
        model: &ModelId,
    ) -> bool {
        if &self.protocol != protocol {
            return false;
        }
        match &self.scope {
            ReplayScope::Protocol => true,
            ReplayScope::ModelFamily => self.model_family.as_ref() == model_family,
            ReplayScope::ExactModel(expected) => expected == model,
        }
    }
}

impl fmt::Debug for ReplayEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayEnvelope")
            .field("protocol", &self.protocol)
            .field("model_family", &self.model_family)
            .field("scope", &self.scope)
            .field("kind", &self.kind)
            .field("version", &self.version)
            .field("classification", &self.classification)
            .field("digest", &self.digest)
            .field("payload", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct ReplayEnvelopeWire {
    protocol: ProtocolId,
    model_family: Option<ModelFamilyId>,
    scope: ReplayScope,
    kind: ReplayKindId,
    version: u32,
    classification: DataClassification,
    digest: Digest,
    payload: Box<RawValue>,
}

impl<'de> Deserialize<'de> for ReplayEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReplayEnvelopeWire::deserialize(deserializer)?;
        let envelope = Self::new(
            wire.protocol,
            wire.model_family,
            wire.scope,
            wire.kind,
            wire.version,
            wire.classification,
            wire.payload,
        )
        .map_err(serde::de::Error::custom)?;
        if envelope.digest != wire.digest {
            return Err(serde::de::Error::custom(ContentError::ReplayDigestMismatch));
        }
        Ok(envelope)
    }
}

fn replay_digest(
    protocol: &ProtocolId,
    model_family: Option<&ModelFamilyId>,
    scope: &ReplayScope,
    kind: &ReplayKindId,
    version: u32,
    classification: DataClassification,
    payload: &RawValue,
) -> Result<Digest, DigestError> {
    let family = model_family.map_or("", ModelFamilyId::as_str);
    let (scope_tag, exact_model) = match scope {
        ReplayScope::Protocol => (&[0_u8][..], ""),
        ReplayScope::ModelFamily => (&[1_u8][..], ""),
        ReplayScope::ExactModel(model) => (&[2_u8][..], model.as_str()),
    };
    let classification_text = match classification {
        DataClassification::Public => "public",
        DataClassification::Sensitive => "sensitive",
        DataClassification::Secret => "secret",
    };
    Digest::sha256(
        DigestDomain::ReplayEnvelope,
        2,
        &[
            protocol.as_str().as_bytes(),
            family.as_bytes(),
            scope_tag,
            exact_model.as_bytes(),
            kind.as_str().as_bytes(),
            &version.to_be_bytes(),
            classification_text.as_bytes(),
            payload.get().as_bytes(),
        ],
    )
}

/// The cross-protocol fallback policy for opaque content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum OpaqueFallback {
    /// Omit the block on incompatible targets.
    Omit,
    /// Preserve only for a compatible replay target.
    CompatibleReplay,
    /// Use this documented semantic summary on incompatible targets.
    SemanticSummary(String),
}

/// A scoped opaque content block.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OpaqueBlock {
    replay: ReplayEnvelope,
    fallback: OpaqueFallback,
}

#[derive(Deserialize)]
struct OpaqueBlockWire {
    replay: ReplayEnvelope,
    fallback: OpaqueFallback,
}

impl<'de> Deserialize<'de> for OpaqueBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OpaqueBlockWire::deserialize(deserializer)?;
        Self::new(wire.replay, wire.fallback).map_err(serde::de::Error::custom)
    }
}

impl OpaqueBlock {
    /// Creates an opaque block with an explicit fallback policy.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when a semantic fallback summary is empty.
    pub fn new(replay: ReplayEnvelope, fallback: OpaqueFallback) -> Result<Self, ContentError> {
        if matches!(&fallback, OpaqueFallback::SemanticSummary(summary) if summary.is_empty()) {
            return Err(ContentError::Empty {
                field: "opaque semantic summary",
            });
        }
        if let OpaqueFallback::SemanticSummary(summary) = &fallback {
            ensure_maximum(
                summary.len(),
                "opaque semantic summary",
                MAX_CANONICAL_BLOCK_BYTES,
            )?;
        }
        Ok(Self { replay, fallback })
    }

    /// Returns scoped replay material.
    #[must_use]
    pub const fn replay(&self) -> &ReplayEnvelope {
        &self.replay
    }

    /// Returns the incompatible-target fallback.
    #[must_use]
    pub const fn fallback(&self) -> &OpaqueFallback {
        &self.fallback
    }

    fn retained_bytes(&self) -> usize {
        self.replay.payload.get().len()
            + match &self.fallback {
                OpaqueFallback::SemanticSummary(summary) => summary.len(),
                OpaqueFallback::Omit | OpaqueFallback::CompatibleReplay => 0,
            }
    }
}

/// Exact producing target for a native compaction item, independent of aggregate continuation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompactionReplayBinding {
    protocol: ProtocolId,
    model_family: Option<ModelFamilyId>,
    model: ModelId,
    account: Digest,
}

impl CompactionReplayBinding {
    /// Records the admitted producing dialect, family, requested model and account.
    #[must_use]
    pub const fn new(
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        model: ModelId,
        account: Digest,
    ) -> Self {
        Self {
            protocol,
            model_family,
            model,
            account,
        }
    }

    /// Proves equality with the current admitted target; freshness is not replay identity.
    #[must_use]
    pub fn is_compatible(
        &self,
        protocol: &ProtocolId,
        model_family: Option<&ModelFamilyId>,
        model: &ModelId,
        account: Digest,
    ) -> bool {
        &self.protocol == protocol
            && self.model_family.as_ref() == model_family
            && &self.model == model
            && self.account == account
    }
}

/// Root turns a compaction removed from canonical history.
///
/// Counted like the turns still in history: `turns` covers every root turn,
/// `prompts` only those a person submitted. The counts include any earlier
/// compaction the removed history already carried, so a history's turn
/// position survives being summarized.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizedTurns {
    prompts: u64,
    turns: u64,
}

impl SummarizedTurns {
    /// Creates counts of summarized root turns; `prompts` never exceeds `turns`.
    #[must_use]
    pub const fn new(prompts: u64, turns: u64) -> Option<Self> {
        if prompts > turns {
            return None;
        }
        Some(Self { prompts, turns })
    }

    /// Returns the summarized turns a person submitted.
    #[must_use]
    pub const fn prompts(self) -> u64 {
        self.prompts
    }

    /// Returns every summarized root turn.
    #[must_use]
    pub const fn turns(self) -> u64 {
        self.turns
    }
}

/// A canonical compaction marker and semantic summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompactionBlock {
    summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_binding: Option<CompactionReplayBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summarized_turns: Option<SummarizedTurns>,
}

#[derive(Deserialize)]
struct CompactionBlockWire {
    summary: String,
    #[serde(default)]
    native_item: Option<String>,
    #[serde(default)]
    native_binding: Option<CompactionReplayBinding>,
    #[serde(default)]
    summarized_turns: Option<SummarizedTurns>,
}

impl<'de> Deserialize<'de> for CompactionBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CompactionBlockWire::deserialize(deserializer)?;
        let mut block = Self::new(wire.summary).map_err(serde::de::Error::custom)?;
        if let Some(native_item) = wire.native_item {
            block = block.with_native_item(native_item);
        }
        block.native_binding = wire.native_binding;
        if let Some(turns) = wire.summarized_turns {
            if turns.prompts > turns.turns {
                return Err(serde::de::Error::custom(
                    "summarized prompts exceed summarized turns",
                ));
            }
            block.summarized_turns = Some(turns);
        }
        Ok(block)
    }
}

impl CompactionBlock {
    /// Creates a non-empty compaction summary.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when the summary is empty.
    pub fn new(summary: impl Into<String>) -> Result<Self, ContentError> {
        let summary = summary.into();
        ensure_nonempty(&summary, "compaction summary")?;
        ensure_maximum(
            summary.len(),
            "compaction summary",
            MAX_CANONICAL_BLOCK_BYTES,
        )?;
        Ok(Self {
            summary,
            native_item: None,
            native_binding: None,
            summarized_turns: None,
        })
    }

    /// Records the root turns this compaction removed from history.
    #[must_use]
    pub const fn with_summarized_turns(mut self, turns: SummarizedTurns) -> Self {
        self.summarized_turns = Some(turns);
        self
    }

    /// Returns the root turns this compaction removed, when it recorded them.
    #[must_use]
    pub const fn summarized_turns(&self) -> Option<SummarizedTurns> {
        self.summarized_turns
    }

    /// Attaches the exact provider compaction JSON for later verbatim replay.
    #[must_use]
    pub fn with_native_item(mut self, native_item: String) -> Self {
        self.native_item = Some(native_item);
        self
    }

    /// Binds native replay to the provider-neutral producing target.
    #[must_use]
    pub fn with_native_binding(mut self, binding: CompactionReplayBinding) -> Self {
        self.native_binding = Some(binding);
        self
    }

    /// Returns the producing binding, if native replay has independently retained authority.
    #[must_use]
    pub const fn native_binding(&self) -> Option<&CompactionReplayBinding> {
        self.native_binding.as_ref()
    }

    /// Returns the semantic summary.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Returns native compaction JSON when this marker is a provider item.
    #[must_use]
    pub fn native_item(&self) -> Option<&str> {
        self.native_item.as_deref()
    }
}

/// A canonical content block whose position is semantically significant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContentBlock {
    /// Text content.
    Text(TextBlock),
    /// Image content.
    Image(ImageBlock),
    /// Document content.
    Document(DocumentBlock),
    /// Audio content.
    Audio(AudioBlock),
    /// Tool call content.
    ToolCall(ToolCallBlock),
    /// Tool result content.
    ToolResult(ToolResultBlock),
    /// Nonterminal tool-result update content.
    ToolResultUpdate(ToolResultUpdateBlock),
    /// Reasoning content.
    Reasoning(ReasoningBlock),
    /// Refusal content.
    Refusal(RefusalBlock),
    /// Citation content.
    Citation(CitationBlock),
    /// Compaction marker content.
    Compaction(CompactionBlock),
    /// Scoped opaque content.
    Opaque(OpaqueBlock),
}

impl ContentBlock {
    pub(crate) fn retained_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.text.len(),
            Self::Image(image) => image.media_type.len() + image.source.retained_bytes(),
            Self::Document(document) => {
                document.media_type.len() + document.source.retained_bytes()
            }
            Self::Audio(audio) => audio.media_type.len() + audio.source.retained_bytes(),
            Self::ToolCall(call) => call.raw_arguments.len(),
            Self::ToolResult(result) => result.parts.iter().fold(0_usize, |total, part| {
                total.saturating_add(part.retained_bytes())
            }),
            Self::ToolResultUpdate(update) => update.text.text.len(),
            Self::Reasoning(reasoning) => {
                reasoning.text.as_deref().map_or(0, str::len)
                    + reasoning
                        .replay
                        .as_ref()
                        .map_or(0, |replay| replay.payload.get().len())
            }
            Self::Refusal(refusal) => refusal.text.len(),
            Self::Citation(citation) => citation.retained_bytes(),
            Self::Compaction(compaction) => compaction.summary.len(),
            Self::Opaque(opaque) => opaque.retained_bytes(),
        }
    }
}

/// Host-only presentation label bound. Not projected to a provider.
const PRESENTATION_SUBJECT_BYTES: usize = 240;

/// One ordered canonical conversation item.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConversationItem {
    id: EntryId,
    actor: Actor,
    blocks: Vec<ContentBlock>,
    provenance: Provenance,
    /// Host-only label for a surface. Never a block, never part of
    /// [`ToolResultBlock`], and omitted from the encoding when absent.
    /// Provider projectors read [`Self::blocks`] and do not send this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    presentation_subject: Option<String>,
}

#[derive(Deserialize)]
struct ConversationItemWire {
    id: EntryId,
    actor: Actor,
    blocks: Vec<ContentBlock>,
    provenance: Provenance,
    #[serde(default)]
    presentation_subject: Option<String>,
}

impl<'de> Deserialize<'de> for ConversationItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ConversationItemWire::deserialize(deserializer)?;
        Self::new(wire.id, wire.actor, wire.blocks, wire.provenance)
            .map(|item| item.with_stored_presentation_subject(wire.presentation_subject))
            .map_err(serde::de::Error::custom)
    }
}

impl ConversationItem {
    /// Creates a conversation item with at least one ordered block.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::EmptyConversationItem`] when no blocks are supplied.
    pub fn new(
        id: EntryId,
        actor: Actor,
        blocks: Vec<ContentBlock>,
        provenance: Provenance,
    ) -> Result<Self, ContentError> {
        if blocks.is_empty() {
            return Err(ContentError::EmptyConversationItem);
        }
        ensure_maximum(
            blocks.len(),
            "conversation item blocks",
            MAX_CONVERSATION_BLOCKS,
        )?;
        for block in &blocks {
            ensure_maximum(
                block.retained_bytes(),
                "content block",
                MAX_CANONICAL_BLOCK_BYTES,
            )?;
        }
        Ok(Self {
            id,
            actor,
            blocks,
            provenance,
            presentation_subject: None,
        })
    }

    /// Records the canonical name of each runtime-owned tool call. A
    /// provider-hosted call keeps the provider's own name as both names.
    pub(crate) fn resolve_canonical_tool_names(
        &mut self,
        resolve: &mut impl FnMut(&ToolCallBlock) -> ToolName,
    ) {
        for block in &mut self.blocks {
            if let ContentBlock::ToolCall(call) = block
                && !matches!(call.owner, ToolExecutionOwner::ProviderHosted { .. })
            {
                call.canonical_name = resolve(call);
            }
        }
    }

    /// Attaches a host-only presentation subject.
    ///
    /// The text is forced onto one line and bounded to 240 bytes with one
    /// trailing ellipsis. Empty input clears it. It is not a content block
    /// and is not part of a tool result.
    #[must_use]
    pub fn with_presentation_subject(mut self, subject: Option<&str>) -> Self {
        self.presentation_subject = subject.and_then(bound_presentation_subject);
        self
    }

    /// The host-only presentation subject, when one was stored.
    #[must_use]
    pub fn presentation_subject(&self) -> Option<&str> {
        self.presentation_subject.as_deref()
    }

    /// Restores a subject exactly as it was encoded, so a history segment's
    /// canonical bytes still match. Bounding happens on the way in.
    fn with_stored_presentation_subject(mut self, subject: Option<String>) -> Self {
        self.presentation_subject = subject.filter(|subject| !subject.is_empty());
        self
    }

    /// Returns the stable entry identity.
    #[must_use]
    pub const fn id(&self) -> EntryId {
        self.id
    }

    /// Returns the responsible actor.
    #[must_use]
    pub const fn actor(&self) -> Actor {
        self.actor
    }

    /// Returns ordered content blocks.
    #[must_use]
    pub fn blocks(&self) -> &[ContentBlock] {
        &self.blocks
    }

    /// Returns ledger provenance.
    #[must_use]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
    }
}

/// One line, 240 bytes, one trailing ellipsis. No redaction: the tool outcome
/// already redacted the text before it was stored.
fn bound_presentation_subject(raw: &str) -> Option<String> {
    let single = raw.trim().replace("\r\n", "↵").replace(['\r', '\n'], "↵");
    let single = single.trim();
    if single.is_empty() {
        return None;
    }
    if single.len() <= PRESENTATION_SUBJECT_BYTES {
        return Some(single.to_owned());
    }
    let ellipsis = "…";
    let mut end = PRESENTATION_SUBJECT_BYTES - ellipsis.len();
    while !single.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}{ellipsis}", &single[..end]))
}

fn ensure_nonempty(value: &str, field: &'static str) -> Result<(), ContentError> {
    if value.is_empty() {
        Err(ContentError::Empty { field })
    } else {
        Ok(())
    }
}

fn ensure_maximum(actual: usize, field: &'static str, maximum: usize) -> Result<(), ContentError> {
    if actual > maximum {
        Err(ContentError::TooLarge { field, maximum })
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use uuid::Uuid;

    #[derive(Deserialize)]
    struct CanonicalItemFixture {
        schema: String,
        version: u32,
        value: ConversationItem,
    }

    fn id(value: u128) -> Result<EntryId, crate::IdentityError> {
        EntryId::try_from(Uuid::from_u128(value))
    }

    #[test]
    fn mixed_block_order_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let item = ConversationItem::new(
            id(1)?,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new("first", Some(TextPhase::Commentary))?),
                ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Summary,
                    Some(String::from("summary")),
                    None,
                )?),
                ContentBlock::Refusal(RefusalBlock::new("cannot")?),
            ],
            Provenance::ModelOutput,
        )?;
        let json = serde_json::to_string(&item)?;
        let decoded: ConversationItem = serde_json::from_str(&json)?;
        assert_eq!(decoded, item);
        Ok(())
    }

    #[test]
    fn replay_debug_redacts_exact_payload() -> Result<(), Box<dyn std::error::Error>> {
        let payload = RawValue::from_string(String::from("{\"secret\":\"do-not-log\"}"))?;
        let envelope = ReplayEnvelope::new(
            ProtocolId::new("test-protocol")?,
            Some(ModelFamilyId::new("test-family")?),
            ReplayScope::ModelFamily,
            ReplayKindId::new("state")?,
            1,
            DataClassification::Sensitive,
            payload,
        )?;
        let debug = format!("{envelope:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("do-not-log"));
        assert!(envelope.is_compatible(
            &ProtocolId::new("test-protocol")?,
            Some(&ModelFamilyId::new("test-family")?),
            &ModelId::new("test-model")?,
        ));
        Ok(())
    }

    #[test]
    fn replay_deserialization_verifies_digest() -> Result<(), Box<dyn std::error::Error>> {
        let payload = RawValue::from_string(String::from("{\"value\":1}"))?;
        let envelope = ReplayEnvelope::new(
            ProtocolId::new("test-protocol")?,
            None,
            ReplayScope::Protocol,
            ReplayKindId::new("state")?,
            1,
            DataClassification::Sensitive,
            payload,
        )?;
        let mut value = serde_json::to_value(envelope)?;
        value["payload"] = serde_json::json!({"value": 2});
        assert!(serde_json::from_value::<ReplayEnvelope>(value).is_err());
        Ok(())
    }

    #[test]
    fn replay_scope_discriminant_is_bound_into_digest() -> Result<(), Box<dyn std::error::Error>> {
        let protocol = ProtocolId::new("test-protocol")?;
        let kind = ReplayKindId::new("state")?;
        let protocol_scoped = ReplayEnvelope::new(
            protocol.clone(),
            None,
            ReplayScope::Protocol,
            kind.clone(),
            1,
            DataClassification::Sensitive,
            RawValue::from_string(String::from("{\"value\":1}"))?,
        )?;
        let exact_model = ReplayEnvelope::new(
            protocol,
            None,
            ReplayScope::ExactModel(ModelId::new("protocol")?),
            kind,
            1,
            DataClassification::Sensitive,
            RawValue::from_string(String::from("{\"value\":1}"))?,
        )?;

        assert_ne!(protocol_scoped.digest(), exact_model.digest());
        Ok(())
    }

    #[test]
    fn deserialization_rechecks_content_constructor_invariants()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut text = serde_json::to_value(TextBlock::new("valid", None)?)?;
        text["text"] = serde_json::json!("");
        assert!(serde_json::from_value::<TextBlock>(text).is_err());

        let mut reasoning = serde_json::to_value(ReasoningBlock::new(
            ReasoningVisibility::Summary,
            Some(String::from("summary")),
            None,
        )?)?;
        reasoning["text"] = Value::Null;
        assert!(serde_json::from_value::<ReasoningBlock>(reasoning).is_err());

        let mut call = serde_json::to_value(ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(10))?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            "{\"value\":1}",
        )?)?;
        call["arguments"] = serde_json::json!({"value": 2});
        assert!(serde_json::from_value::<ToolCallBlock>(call).is_err());

        let mut item = serde_json::to_value(ConversationItem::new(
            id(11)?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("valid", None)?)],
            Provenance::ModelOutput,
        )?)?;
        item["blocks"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ConversationItem>(item).is_err());
        Ok(())
    }

    /// Canonical history keeps both names of an aliased call through its
    /// encoding; a call built from one emitted name records it as both.
    #[test]
    fn an_aliased_tool_call_round_trips_both_names() -> Result<(), Box<dyn std::error::Error>> {
        let call = ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(30))?,
            ToolName::new("edit")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            "{}",
        )?;
        assert_eq!(call.canonical_name(), call.exposed_name());
        let call = call.with_canonical_name(ToolName::new("apply_patch")?);
        let value = serde_json::to_value(&call)?;
        assert_eq!(value["canonical_name"], "apply_patch");
        assert_eq!(value["exposed_name"], "edit");
        assert!(value.get("tool").is_none());
        let decoded: ToolCallBlock = serde_json::from_value(value)?;
        assert_eq!(decoded, call);
        assert_eq!(decoded.canonical_name().as_str(), "apply_patch");
        assert_eq!(decoded.exposed_name().as_str(), "edit");
        Ok(())
    }

    #[test]
    fn tool_result_updates_and_exact_media_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let call_id = CallId::try_from(Uuid::from_u128(12))?;
        let correlation =
            ToolCorrelation::new(Some("remote-exec".into()), None, ToolCallerLineage::Direct)?;
        let update = ToolResultUpdateBlock::new(
            call_id,
            ToolExecutionOwner::PlakLocal,
            TextBlock::new("still running", None)?,
            correlation,
        )?;
        let decoded: ToolResultUpdateBlock = serde_json::from_slice(&serde_json::to_vec(&update)?)?;
        assert_eq!(decoded, update);

        let data_url = format!("data:image/png;base64,{}", "A".repeat(9 * 1024));
        let image = ImageBlock::with_detail(
            "image/png",
            MediaSource::data_url(data_url.clone())?,
            Some(ImageDetailHint::Original),
        )?;
        let decoded: ImageBlock = serde_json::from_slice(&serde_json::to_vec(&image)?)?;
        assert_eq!(decoded.source().as_data_url(), Some(data_url.as_str()));
        assert_eq!(decoded.detail(), Some(ImageDetailHint::Original));
        assert!(MediaSource::data_url("https://example.invalid/image").is_err());
        assert!(
            ToolResultUpdateBlock::new(
                call_id,
                ToolExecutionOwner::ProviderHosted {
                    provider_kind: NamespaceId::new("hosted")?,
                },
                TextBlock::new("invalid", None)?,
                ToolCorrelation::default(),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn caller_lineage_is_closed_and_independent_from_execution_owner()
    -> Result<(), Box<dyn std::error::Error>> {
        let programmatic = ToolCallerLineage::provider_programmatic(
            NamespaceId::new("hosted.program")?,
            "program-call-7",
        )?;
        let local_programmatic = ToolCallerLineage::local_programmatic(
            41,
            7,
            "cell-7",
            3,
            CallId::try_from(Uuid::from_u128(23))?,
            1,
        )?;
        let local = ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(20))?,
            ToolName::new("local_lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Pending,
            ToolCorrelation::new(None, None, programmatic.clone())?,
            r#"{"query":"local"}"#,
        )?;
        let mcp = ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(21))?,
            ToolName::new("mcp_lookup")?,
            ToolExecutionOwner::PlakMcp {
                server: McpServerId::new("remote-catalog")?,
            },
            ToolCallState::Pending,
            ToolCorrelation::new(Some("remote-21".into()), None, local_programmatic.clone())?,
            r#"{"query":"mcp"}"#,
        )?;
        let hosted = ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(22))?,
            ToolName::new("hosted_interpreter")?,
            ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new("hosted.interpreter")?,
            },
            ToolCallState::Completed,
            ToolCorrelation::new(None, Some("container-22".into()), ToolCallerLineage::Direct)?,
            "{}",
        )?;

        assert_eq!(local.correlation().caller_lineage(), &programmatic);
        assert_eq!(mcp.correlation().caller_lineage(), &local_programmatic);
        assert!(matches!(
            local_programmatic,
            ToolCallerLineage::LocalProgrammatic { depth: 1, .. }
        ));
        assert_eq!(
            hosted.correlation().caller_lineage(),
            &ToolCallerLineage::Direct
        );
        assert!(matches!(local.owner(), ToolExecutionOwner::PlakLocal));
        assert!(matches!(mcp.owner(), ToolExecutionOwner::PlakMcp { .. }));
        assert!(matches!(
            hosted.owner(),
            ToolExecutionOwner::ProviderHosted { .. }
        ));

        let decoded: ToolCallBlock = serde_json::from_slice(&serde_json::to_vec(&mcp)?)?;
        assert_eq!(decoded, mcp);
        assert!(
            ToolCallerLineage::provider_programmatic(NamespaceId::new("hosted.program")?, "")
                .is_err()
        );
        assert!(
            ToolCallerLineage::local_programmatic(
                41,
                7,
                "cell-7",
                3,
                CallId::try_from(Uuid::from_u128(24))?,
                0,
            )
            .is_err()
        );
        let obsolete = serde_json::json!({
            "remote_call_id": null,
            "container_id": null,
            "caller_id": "untyped"
        });
        assert!(serde_json::from_value::<ToolCorrelation>(obsolete).is_err());
        Ok(())
    }

    #[test]
    fn local_programmatic_lineage_requires_generation_and_cell()
    -> Result<(), Box<dyn std::error::Error>> {
        let call_id = CallId::try_from(Uuid::from_u128(24))?;
        assert!(ToolCallerLineage::local_programmatic(41, 0, "cell-7", 3, call_id, 1).is_err());
        assert!(ToolCallerLineage::local_programmatic(41, 7, "", 3, call_id, 1).is_err());

        let lineage = ToolCallerLineage::local_programmatic(41, 7, "cell-7", 3, call_id, 1)?;
        let mut missing_generation = serde_json::to_value(lineage)?;
        missing_generation
            .as_object_mut()
            .expect("lineage object")
            .get_mut("local_programmatic")
            .and_then(Value::as_object_mut)
            .expect("local lineage fields")
            .remove("interpreter_generation");
        assert!(serde_json::from_value::<ToolCallerLineage>(missing_generation).is_err());
        Ok(())
    }

    #[test]
    fn versioned_canonical_item_fixture_is_stable() -> Result<(), Box<dyn std::error::Error>> {
        let fixture: CanonicalItemFixture =
            serde_json::from_str(include_str!("../tests/fixtures/canonical_item_v1.json"))?;
        assert_eq!(fixture.schema, "plak.canonical-conversation-item");
        assert_eq!(fixture.version, 1);
        assert_eq!(fixture.value.id(), id(1)?);
        assert_eq!(fixture.value.blocks().len(), 1);
        let encoded = serde_json::to_string(&fixture.value)?;
        let decoded: ConversationItem = serde_json::from_str(&encoded)?;
        assert_eq!(decoded, fixture.value);
        Ok(())
    }
}
