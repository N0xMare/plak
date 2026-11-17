//! Native reasoning loses its authority after the content it reasoned over changes.

use super::{CanonicalHistory, HistoryError, HistorySegment, HistorySegmentKind};
use crate::{ContentBlock, ConversationItem};

impl CanonicalHistory {
    /// Removes native reasoning after the first content rewrite relative to `source`.
    ///
    /// An unchanged prefix, including its reasoning, is shared exactly. Appended
    /// content does not invalidate earlier reasoning. Once a prefix changes, all
    /// subsequent replay-bearing reasoning is omitted, never converted to text;
    /// other content and tool correlations remain intact.
    ///
    /// # Errors
    ///
    /// Returns a history validation error if a rewritten group is invalid.
    pub fn without_invalidated_reasoning(&self, source: &Self) -> Result<Self, HistoryError> {
        let source_segments = source.segments();
        let mut originals = source_segments.iter().flat_map(HistorySegment::items);
        let mut changed_prefix = false;
        let mut changed_history = false;
        let mut segments = Vec::with_capacity(self.segments().len());
        for segment in self.segments() {
            let mut changed_segment = false;
            let mut items = Vec::with_capacity(segment.items().len());
            for item in segment.items() {
                let original = originals.next();
                changed_prefix |= original.is_some_and(|old| old.actor() != item.actor());
                let mut blocks = Vec::with_capacity(item.blocks().len());
                for (index, block) in item.blocks().iter().enumerate() {
                    changed_prefix |= original
                        .and_then(|old| old.blocks().get(index))
                        .is_some_and(|old| old != block);
                    if changed_prefix
                        && matches!(block, ContentBlock::Reasoning(reasoning) if reasoning.replay().is_some())
                    {
                        changed_segment = true;
                    } else {
                        blocks.push(block.clone());
                    }
                }
                changed_prefix |=
                    original.is_some_and(|old| old.blocks().len() != item.blocks().len());
                if blocks.len() == item.blocks().len() {
                    items.push(item.clone());
                } else if !blocks.is_empty() {
                    items.push(
                        ConversationItem::new(item.id(), item.actor(), blocks, item.provenance())
                            .map_err(|error| HistoryError::Serialization(error.to_string()))?
                            .with_presentation_subject(item.presentation_subject()),
                    );
                }
            }
            if !changed_segment {
                segments.push(segment.clone());
                continue;
            }
            changed_history = true;
            if !items.is_empty() {
                let kind = if segment.kind() == HistorySegmentKind::ReasoningReplay
                    && !items
                        .iter()
                        .flat_map(ConversationItem::blocks)
                        .any(|block| {
                            matches!(block, ContentBlock::Reasoning(_) | ContentBlock::Opaque(_))
                        }) {
                    HistorySegmentKind::Conversation
                } else {
                    segment.kind()
                };
                segments.push(HistorySegment::new(kind, items)?);
            }
        }
        if changed_history {
            Self::new(segments)
        } else {
            Ok(self.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Actor, DataClassification, EntryId, ProtocolId, Provenance, ReasoningBlock,
        ReasoningVisibility, ReplayEnvelope, ReplayKindId, ReplayScope, TextBlock,
    };
    use serde_json::value::RawValue;
    use uuid::Uuid;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn reasoning() -> TestResult<ContentBlock> {
        Ok(ContentBlock::Reasoning(ReasoningBlock::new(
            ReasoningVisibility::Omitted,
            None,
            Some(ReplayEnvelope::new(
                ProtocolId::new("fixture.protocol")?,
                None,
                ReplayScope::Protocol,
                ReplayKindId::new("fixture.reasoning")?,
                1,
                DataClassification::Sensitive,
                RawValue::from_string("\"synthetic\"".into())?,
            )?),
        )?))
    }

    fn segment(id: u128, blocks: Vec<ContentBlock>) -> TestResult<HistorySegment> {
        let kind = if blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Reasoning(_)))
        {
            HistorySegmentKind::ReasoningReplay
        } else {
            HistorySegmentKind::Conversation
        };
        Ok(HistorySegment::new(
            kind,
            vec![ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(id))?,
                Actor::Assistant,
                blocks,
                Provenance::ModelOutput,
            )?],
        )?)
    }

    fn text(value: &str) -> TestResult<ContentBlock> {
        Ok(ContentBlock::Text(TextBlock::new(value, None)?))
    }

    #[test]
    fn rewrite_keeps_reasoning_before_the_change_and_drops_the_entire_native_suffix() -> TestResult
    {
        let first = segment(1, vec![reasoning()?, text("original")?])?;
        let only_reasoning = segment(2, vec![reasoning()?])?;
        let last = segment(3, vec![reasoning()?, text("answer")?])?;
        let source =
            CanonicalHistory::new(vec![first.clone(), only_reasoning.clone(), last.clone()])?;
        let rewritten = CanonicalHistory::new(vec![
            segment(1, vec![reasoning()?, text("changed")?])?,
            only_reasoning,
            last,
        ])?;
        let projected = rewritten.without_invalidated_reasoning(&source)?;
        assert_eq!(projected.item_count(), 2);
        assert_eq!(
            projected.segments().get(0).ok_or("first")?.items()[0].blocks()[0],
            first.items()[0].blocks()[0]
        );
        assert_eq!(
            projected.segments().get(1).ok_or("last")?.items()[0].blocks(),
            &[text("answer")?]
        );
        assert_eq!(source.item_count(), 3);
        Ok(())
    }

    #[test]
    fn append_only_and_post_cut_reasoning_keep_exact_storage() -> TestResult {
        let source = CanonicalHistory::new(vec![segment(1, vec![text("prefix")?, reasoning()?])?])?;
        let appended = source.append(segment(2, vec![reasoning()?, text("answer")?])?)?;
        let projected = appended.without_invalidated_reasoning(&source)?;
        assert_eq!(projected, appended);
        assert!(
            projected
                .segments()
                .get(0)
                .ok_or("first")?
                .shares_storage_with(source.segments().get(0).ok_or("source")?)
        );
        let cut = CanonicalHistory::new(vec![segment(3, vec![text("summary")?])?])?;
        let new_round = cut.append(segment(4, vec![reasoning()?, text("new answer")?])?)?;
        assert_eq!(new_round.without_invalidated_reasoning(&cut)?, new_round);
        Ok(())
    }
}
