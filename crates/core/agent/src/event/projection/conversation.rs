//! Content-visible summaries carried by conversation commit events.
use plakous_model::{Actor, EntryId, Provenance};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
/// Presentation-safe summary of one committed canonical conversation item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConversationItemView {
    entry_id: EntryId,
    actor: Actor,
    provenance: Provenance,
    block_count: usize,
    display_text: Option<Arc<str>>,
    tool_calls: usize,
    tool_results: usize,
    has_non_display_content: bool,
}

impl ConversationItemView {
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

    /// Returns the canonical provenance.
    #[must_use]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// Returns the canonical block count.
    #[must_use]
    pub const fn block_count(&self) -> usize {
        self.block_count
    }

    /// Returns displayable text with replay/raw/opaque material excluded.
    #[must_use]
    pub fn display_text(&self) -> Option<&str> {
        self.display_text.as_deref()
    }

    /// Returns the number of tool-call blocks without exposing arguments.
    #[must_use]
    pub const fn tool_calls(&self) -> usize {
        self.tool_calls
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
}
