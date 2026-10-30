//! A session's committed conversation, carried out of one durable session to
//! seed a new one.
//!
//! The carrier holds committed history and named JSON values, but no live
//! operation ledger, context anchors, family artifacts or provider
//! continuation. Provider-opaque replay material (encrypted reasoning,
//! thinking signatures, opaque items, native compaction items) travels with it
//! but seeds the new session only when the seeding runtime binds the same
//! account and model the material was produced under; otherwise it is
//! stripped, as a target that could not have produced it would refuse it.

use plakous_model::{
    CanonicalHistory, CompactionBlock, ContentBlock, ConversationItem, HistorySegment,
    HistorySegmentKind, ModelId, OpaqueFallback, ReasoningBlock, ReasoningVisibility, TextBlock,
    ToolResultBlock, ToolResultPart, hex_lower,
};

use super::{AgentSessionsError, AgentSessionsErrorKind, SessionRuntimeBinding};
use plakous_agent::internal::facade_support::agent_builder_target_authority;

/// The committed canonical conversation of one quiescent session head, plus
/// the account and model its provider replay material is bound to, and its
/// committed session-owned named values.
#[derive(Clone, Debug, PartialEq)]
pub struct PortableSessionHistory {
    pub(super) named_state: plakous_agent::internal::NamedState,
    segments: Vec<HistorySegment>,
    replay_account: String,
    replay_model: ModelId,
    pub(super) validate_replay: bool,
}

impl PortableSessionHistory {
    /// Reassembles a carried history.
    ///
    /// `replay_account` is the account reference the source runtime admitted
    /// (lowercase hex of its account binding) and `replay_model` the model it
    /// requested; both only decide whether replay material is kept.
    #[must_use]
    pub fn new(
        segments: Vec<HistorySegment>,
        replay_account: String,
        replay_model: ModelId,
    ) -> Self {
        Self {
            named_state: plakous_agent::internal::NamedState::default(),
            segments,
            replay_account,
            replay_model,
            validate_replay: false,
        }
    }

    /// Attaches session-owned named JSON values, validated before creation.
    ///
    /// # Errors
    /// Refuses invalid keys, non-interoperable values, or named-state quotas.
    pub fn with_named_state(
        mut self,
        values: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<Self, AgentSessionsError> {
        let mut state = plakous_agent::internal::NamedState::default();
        for (key, value) in values {
            state = state
                .put(key, &value)
                .map_err(|error| invalid_history(&error))?;
        }
        self.named_state = state;
        Ok(self)
    }

    /// The session-owned named values as portable JSON, ordered by key.
    ///
    /// # Errors
    /// Returns invalid content if a retained canonical value cannot be decoded.
    pub fn named_state(
        &self,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, AgentSessionsError> {
        self.named_state
            .values()
            .iter()
            .map(|(key, bytes)| {
                serde_json::from_slice(bytes)
                    .map(|value| (key.clone(), value))
                    .map_err(|error| invalid_history(&error))
            })
            .collect()
    }

    /// Removes all provider replay from an externally supplied document.
    /// The declared binding of a document is not authenticated authority.
    ///
    /// # Errors
    /// Returns invalid content when a portable fallback cannot be reconstructed.
    pub fn without_replay(mut self) -> Result<Self, AgentSessionsError> {
        self.segments = carried_segments_without_replay(&self.segments)?;
        self.replay_account.clear();
        Ok(self)
    }

    /// Checks retained replay with the destination's offline dialect decoder.
    /// A refusal creates with stripped history, reported by
    /// [`super::OpenedSession::replay_validation_error`]. Use only for a locally attested handoff.
    #[must_use]
    pub fn with_replay_validation(mut self) -> Self {
        self.validate_replay = true;
        self
    }

    /// The committed segments, oldest first.
    #[must_use]
    pub fn segments(&self) -> &[HistorySegment] {
        &self.segments
    }

    /// The source account reference replay material is bound to.
    #[must_use]
    pub fn replay_account(&self) -> &str {
        &self.replay_account
    }

    /// The source model replay material is bound to.
    #[must_use]
    pub const fn replay_model(&self) -> &ModelId {
        &self.replay_model
    }

    /// Whether seeding a session through `runtime` keeps the provider replay
    /// material: only when it binds the same account and the same model.
    #[must_use]
    pub fn retains_replay_for(&self, runtime: &SessionRuntimeBinding) -> bool {
        let (_, _, model, _, _, account) = agent_builder_target_authority(&runtime.builder);
        model == self.replay_model && hex_lower(account.bytes()) == self.replay_account
    }

    pub(super) fn into_canonical(
        self,
        retain_replay: bool,
    ) -> Result<CanonicalHistory, AgentSessionsError> {
        let segments = if retain_replay {
            self.segments
        } else {
            carried_segments_without_replay(&self.segments)?
        };
        CanonicalHistory::new(segments).map_err(|error| invalid_history(&error))
    }
}

fn invalid_history(error: &dyn std::fmt::Display) -> AgentSessionsError {
    AgentSessionsError::new(
        AgentSessionsErrorKind::InvalidInput,
        format!("carried session history is invalid: {error}"),
    )
}

/// Strips replay, then places a carried native compaction at position zero.
///
/// A native successor is `[retained prefix..., summary, current-round
/// tail...]`, and later rounds follow the tail. The summary stands in for
/// context before that prefix (the carried marker says earlier context could
/// not be carried; the prefix is the verbatim user, developer and system
/// messages). A semantic cut accepts a prior compaction only at position zero,
/// so the plain summary has to lead; the tail and later rounds keep their
/// order behind the prefix. Turn position is the sum of the compaction record
/// and the root turns still outside it, so the move keeps `(prompts, turns)`.
/// Same-binding carry keeps the native item and this order: Codex still
/// replays the summary after the retained prefix and before the tail.
fn carried_segments_without_replay(
    segments: &[HistorySegment],
) -> Result<Vec<HistorySegment>, AgentSessionsError> {
    let mut carried = Vec::with_capacity(segments.len());
    let mut native_summary = None;
    for segment in segments {
        let native = has_native_compaction(segment);
        let Some(stripped) = segment_without_replay(segment)? else {
            continue;
        };
        if native && stripped.kind() == HistorySegmentKind::Compaction && native_summary.is_none() {
            native_summary = Some(carried.len());
        }
        carried.push(stripped);
    }
    if let Some(index) = native_summary.filter(|index| *index > 0) {
        let summary = carried.remove(index);
        carried.insert(0, summary);
    }
    Ok(carried)
}

fn has_native_compaction(segment: &HistorySegment) -> bool {
    segment
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .any(|block| match block {
            ContentBlock::Compaction(compaction) => compaction.native_item().is_some(),
            _ => false,
        })
}

/// The segment with every replay envelope removed, or `None` when nothing
/// model-visible is left of it. A reasoning-replay group left with no
/// reasoning at all is ordinary conversation afterwards.
fn segment_without_replay(
    segment: &HistorySegment,
) -> Result<Option<HistorySegment>, AgentSessionsError> {
    let items = segment
        .items()
        .iter()
        .map(item_without_replay)
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, _>>()?;
    if items.is_empty() {
        return Ok(None);
    }
    let reasoning_left = items
        .iter()
        .flat_map(ConversationItem::blocks)
        .any(|block| matches!(block, ContentBlock::Reasoning(_)));
    let kind = match segment.kind() {
        HistorySegmentKind::ReasoningReplay if !reasoning_left => HistorySegmentKind::Conversation,
        HistorySegmentKind::Compaction
            if !items
                .iter()
                .flat_map(ConversationItem::blocks)
                .any(|block| matches!(block, ContentBlock::Compaction(_))) =>
        {
            HistorySegmentKind::Conversation
        }
        kind => kind,
    };
    HistorySegment::new(kind, items)
        .map(Some)
        .map_err(|error| invalid_history(&error))
}

fn item_without_replay(
    item: &ConversationItem,
) -> Result<Option<ConversationItem>, AgentSessionsError> {
    let blocks = item
        .blocks()
        .iter()
        .map(block_without_replay)
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, _>>()?;
    if blocks.is_empty() {
        return Ok(None);
    }
    // The subject is host-only, not replay material. Rebuilding the item must
    // keep it; provider projectors still read blocks only.
    ConversationItem::new(item.id(), item.actor(), blocks, item.provenance())
        .map(|built| built.with_presentation_subject(item.presentation_subject()))
        .map(Some)
        .map_err(|error| invalid_history(&error))
}

fn block_without_replay(block: &ContentBlock) -> Result<Option<ContentBlock>, AgentSessionsError> {
    let stripped = match block {
        ContentBlock::Reasoning(reasoning) if reasoning.replay().is_some() => {
            match reasoning.visibility() {
                ReasoningVisibility::OpaqueReplay => return Ok(None),
                visibility => {
                    ReasoningBlock::new(visibility, reasoning.text().map(str::to_owned), None)
                        .map(ContentBlock::Reasoning)
                }
            }
        }
        ContentBlock::ToolResult(result)
            if result
                .parts()
                .iter()
                .any(|part| matches!(part, ToolResultPart::Opaque(_))) =>
        {
            tool_result_without_replay(result)
        }
        ContentBlock::Opaque(opaque) => match opaque.fallback() {
            OpaqueFallback::SemanticSummary(summary) => {
                TextBlock::new(summary.clone(), None).map(ContentBlock::Text)
            }
            _ => return Ok(None),
        },
        // A plain compaction summary, not text: it keeps the turns the native
        // compaction recorded. `carried_segments_without_replay` then leads
        // with that segment so a later semantic cut can fold it.
        ContentBlock::Compaction(compaction) if compaction.native_item().is_some() => {
            CompactionBlock::new(
                "[earlier context was natively compacted and cannot be carried to this binding]",
            )
            .map(|carried| match compaction.summarized_turns() {
                Some(turns) => carried.with_summarized_turns(turns),
                None => carried,
            })
            .map(ContentBlock::Compaction)
        }
        block => return Ok(Some(block.clone())),
    };
    stripped.map(Some).map_err(|error| invalid_history(&error))
}

fn tool_result_without_replay(
    result: &ToolResultBlock,
) -> Result<ContentBlock, plakous_model::ContentError> {
    let mut parts = result
        .parts()
        .iter()
        .map(|part| match part {
            ToolResultPart::Opaque(opaque) => match opaque.fallback() {
                OpaqueFallback::SemanticSummary(summary) => TextBlock::new(summary.clone(), None)
                    .map(ToolResultPart::Text)
                    .map(Some),
                _ => Ok(None),
            },
            part => Ok(Some(part.clone())),
        })
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, _>>()?;
    // Keep the terminal result so the carried exchange remains complete.
    if parts.is_empty() {
        parts.push(ToolResultPart::Text(TextBlock::new(
            "[provider replay omitted at carry boundary]",
            None,
        )?));
    }
    ToolResultBlock::new(
        result.call_id(),
        result.owner().clone(),
        result.kind(),
        parts,
        result.correlation().clone(),
    )
    .map(ContentBlock::ToolResult)
}

#[cfg(test)]
mod tests;
