use super::{
    Actor, CallId, CheckpointId, Digest, EntryId, Provenance, PruneHistoryReport,
    SessionHistoryCompaction, SessionHistoryCompactionPass, SessionHistoryCursor,
    SessionHistoryDirection, SessionHistoryItem, SessionHistoryPageRequest,
    SessionHistoryToolBlock, SessionHistoryVisibility, SessionLocator, ToolName, fmt,
};

impl SessionHistoryCursor {
    /// Returns the exact checkpoint this cursor was minted against.
    #[must_use]
    pub const fn locator(self) -> SessionLocator {
        self.locator
    }

    /// Returns the canonical-history identity bound into this cursor.
    #[must_use]
    pub const fn history_identity(self) -> Digest {
        self.history_identity
    }

    /// Returns the next item index in canonical order.
    #[must_use]
    pub const fn next_index(self) -> u64 {
        self.next_index
    }
}

impl SessionHistoryPageRequest {
    /// Binds one bounded history page to an exact checkpoint and visibility revision.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "history page identity is an exact eight-field request, not a decomposed builder"
    )]
    pub const fn new(
        locator: SessionLocator,
        checkpoint_id: CheckpointId,
        visibility: SessionHistoryVisibility,
        visibility_revision: u32,
        direction: SessionHistoryDirection,
        max_items: u32,
        max_bytes: u32,
        cursor: Option<SessionHistoryCursor>,
    ) -> Self {
        Self {
            locator,
            checkpoint_id,
            visibility,
            visibility_revision,
            direction,
            max_items,
            max_bytes,
            cursor,
        }
    }
}

impl fmt::Debug for SessionHistoryPageRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHistoryPageRequest")
            .field("locator", &self.locator)
            .field("checkpoint_id", &self.checkpoint_id)
            .field("visibility", &self.visibility)
            .field("visibility_revision", &self.visibility_revision)
            .field("direction", &self.direction)
            .field("max_items", &self.max_items)
            .field("max_bytes", &self.max_bytes)
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl SessionHistoryToolBlock {
    /// Returns the canonical tool-call identity this block belongs to.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the exact name the model called for a call block, absent for a
    /// result block.
    #[must_use]
    pub const fn tool(&self) -> Option<&ToolName> {
        self.tool.as_ref()
    }
}

impl SessionHistoryCompactionPass {
    /// Stable parenthetical used in the compaction display phrase.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SemanticSummary => "semantic summary",
            Self::Placeholder => "placeholder pass",
            Self::Native => "native compaction",
            Self::ReplayOmission => "replay omission",
        }
    }
}

impl SessionHistoryItem {
    /// Returns the canonical conversation-entry identity.
    #[must_use]
    pub const fn entry_id(&self) -> EntryId {
        self.entry_id
    }
    /// Returns the responsible actor.
    #[must_use]
    pub const fn actor(&self) -> Actor {
        self.actor
    }
    /// Returns how this item entered the conversation.
    ///
    /// A steering segment must carry `Actor::User` to lower as a user-role
    /// message on every protocol, so the actor alone cannot distinguish a note
    /// the harness injected mid-turn from text the operator typed. Provenance
    /// can: the harness writes `Instruction`, typed input is `UserInput`.
    #[must_use]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
    }
    /// True when the harness authored this item rather than the operator.
    #[must_use]
    pub const fn is_harness_authored(&self) -> bool {
        matches!(self.actor, Actor::User) && matches!(self.provenance, Provenance::Instruction)
    }
    /// Returns the canonical block count.
    #[must_use]
    pub const fn block_count(&self) -> usize {
        self.block_count
    }
    /// Returns displayable text when content visibility is granted.
    #[must_use]
    pub fn display_text(&self) -> Option<&str> {
        self.display_text.as_deref()
    }
    /// Returns the number of tool-call blocks without exposing arguments.
    #[must_use]
    pub const fn tool_calls(&self) -> usize {
        self.tool_calls
    }
    /// Returns this item's tool blocks in canonical order, named but payloadless.
    #[must_use]
    pub fn tool_blocks(&self) -> &[SessionHistoryToolBlock] {
        &self.tool_blocks
    }
    /// Returns the number of tool-result blocks without exposing result content.
    #[must_use]
    pub const fn tool_results(&self) -> usize {
        self.tool_results
    }
    /// Reports whether non-display content was intentionally omitted.
    #[must_use]
    pub const fn has_non_display_content(&self) -> bool {
        self.has_non_display_content
    }
    /// Returns the compaction projection when this item's segment is one.
    ///
    /// `None` for every other segment. The summary and provider replay are
    /// not recoverable from this value.
    #[must_use]
    pub const fn compaction(&self) -> Option<SessionHistoryCompaction> {
        self.compaction
    }
    /// The history-page kind: `compaction` for a committed compaction segment,
    /// otherwise the actor name.
    #[must_use]
    pub const fn projected_kind(&self) -> &'static str {
        if self.compaction.is_some() {
            "compaction"
        } else {
            self.actor.as_str()
        }
    }
}

impl fmt::Debug for SessionHistoryItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHistoryItem")
            .field("entry_id", &self.entry_id)
            .field("actor", &self.actor)
            .field("provenance", &self.provenance)
            .field("block_count", &self.block_count)
            .field(
                "display_text",
                &self.display_text.as_ref().map(|_| "<redacted>"),
            )
            .field("tool_calls", &self.tool_calls)
            .field("tool_blocks", &self.tool_blocks)
            .field("tool_results", &self.tool_results)
            .field("has_non_display_content", &self.has_non_display_content)
            .field("compaction", &self.compaction)
            .finish()
    }
}

impl PruneHistoryReport {
    /// Returns the unchanged head the prune ran at.
    #[must_use]
    pub const fn locator(self) -> SessionLocator {
        self.locator
    }
    /// Returns removed superseded checkpoints.
    #[must_use]
    pub const fn pruned_checkpoints(self) -> u64 {
        self.pruned_checkpoints
    }
}
