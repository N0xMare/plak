//! Ordered canonical conversation, content, and replay values.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{DigestError, EntryId, McpServerId, ToolName};

mod citation;
mod media;
mod replay;
mod text;
mod tool;

pub use citation::{CitationBlock, CitationRange, CitationSource, CitationTarget, CitationUnit};
pub use media::{AudioBlock, DocumentBlock, ImageBlock, ImageDetailHint, MediaSource};
pub use replay::{
    CompactionBlock, CompactionReplayBinding, DataClassification, OpaqueBlock, OpaqueFallback,
    ReplayEnvelope, ReplayScope, SummarizedTurns,
};
pub use text::{ReasoningBlock, ReasoningVisibility, RefusalBlock, TextBlock, TextPhase};
pub use tool::{
    ToolCallBlock, ToolCallState, ToolCallerLineage, ToolCorrelation, ToolExecutionOwner,
    ToolResultBlock, ToolResultKind, ToolResultPart, ToolResultUpdateBlock,
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
            Self::Text(text) => text.text().len(),
            Self::Image(image) => image.media_type().len() + image.source().retained_bytes(),
            Self::Document(document) => {
                document.media_type().len() + document.source().retained_bytes()
            }
            Self::Audio(audio) => audio.media_type().len() + audio.source().retained_bytes(),
            Self::ToolCall(call) => call.raw_arguments().len(),
            Self::ToolResult(result) => result.parts().iter().fold(0_usize, |total, part| {
                total.saturating_add(part.retained_bytes())
            }),
            Self::ToolResultUpdate(update) => update.text().text().len(),
            Self::Reasoning(reasoning) => {
                reasoning.text().map_or(0, str::len)
                    + reasoning
                        .replay()
                        .map_or(0, |replay| replay.payload().get().len())
            }
            Self::Refusal(refusal) => refusal.text().len(),
            Self::Citation(citation) => citation.retained_bytes(),
            Self::Compaction(compaction) => compaction.summary().len(),
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

    /// Records the canonical name and runtime owner of each runtime-owned
    /// tool call: `None` is Plak-local, `Some(server)` that server's MCP owner.
    /// A provider-hosted call keeps the provider's own name and owner.
    pub(crate) fn resolve_canonical_tool_calls(
        &mut self,
        resolve: &mut impl FnMut(&ToolCallBlock) -> (ToolName, Option<McpServerId>),
    ) {
        for block in &mut self.blocks {
            if let ContentBlock::ToolCall(call) = block
                && !matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. })
            {
                let (canonical_name, server) = resolve(call);
                call.resolve_runtime(
                    canonical_name,
                    server.map_or(ToolExecutionOwner::Local, |server| {
                        ToolExecutionOwner::Mcp { server }
                    }),
                );
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
mod tests;
