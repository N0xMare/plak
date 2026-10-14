//! A session's committed conversation, carried out of one durable session to
//! seed a new one.
//!
//! The carrier holds canonical history segments and nothing a store owns: no
//! operation ledger, context anchors, family artifacts or provider
//! continuation. Provider-opaque replay material (encrypted reasoning,
//! thinking signatures, opaque items, native compaction items) travels with it
//! but seeds the new session only when the seeding runtime binds the same
//! account and model the material was produced under; otherwise it is
//! stripped, as a target that could not have produced it would refuse it.

use plakous_model::{
    CanonicalHistory, CompactionBlock, ContentBlock, ConversationItem, HistorySegment,
    HistorySegmentKind, ModelId, OpaqueFallback, ReasoningBlock, ReasoningVisibility, TextBlock,
    hex_lower,
};

use super::{AgentSessionsError, AgentSessionsErrorKind, SessionRuntimeBinding};
use plakous_agent::internal::facade_support::agent_builder_target_authority;

/// The committed canonical conversation of one quiescent session head, plus
/// the account and model its provider replay material is bound to.
#[derive(Clone, Debug, PartialEq)]
pub struct PortableSessionHistory {
    segments: Vec<HistorySegment>,
    replay_account: String,
    replay_model: ModelId,
}

impl PortableSessionHistory {
    /// Reassembles a carried history.
    ///
    /// `replay_account` is the account reference the source runtime admitted
    /// (lowercase hex of its account binding) and `replay_model` the model it
    /// requested; both only decide whether replay material is kept.
    #[must_use]
    pub const fn new(
        segments: Vec<HistorySegment>,
        replay_account: String,
        replay_model: ModelId,
    ) -> Self {
        Self {
            segments,
            replay_account,
            replay_model,
        }
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
/// A native successor is `[retained prefix..., summary]`. The summary stands
/// in for context before that prefix (the carried marker says earlier context
/// could not be carried; the prefix is the verbatim user, developer and system
/// tail). A semantic cut accepts a prior compaction only at position zero, so
/// the plain summary has to lead. Turn position is the sum of the compaction
/// record and the root turns still outside it, so the move keeps
/// `(prompts, turns)`. Same-binding carry keeps the native item and this
/// order: Codex still replays the summary last.
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

#[cfg(test)]
mod tests {
    use plakous_model::{
        Actor, CompactionBlock, ContentBlock, ConversationItem, EntryId, HistorySegment,
        HistorySegmentKind, ModelId, Provenance, SummarizedTurns, TextBlock,
    };

    use super::PortableSessionHistory;

    fn item(subject: Option<&str>) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        let built = ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(7))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?;
        Ok(built.with_presentation_subject(subject))
    }

    #[test]
    fn carry_without_replay_keeps_the_presentation_subject()
    -> Result<(), Box<dyn std::error::Error>> {
        let segment =
            HistorySegment::new(HistorySegmentKind::Conversation, vec![item(Some("c1,c2"))?])?;
        let carried = PortableSessionHistory::new(
            vec![segment],
            "00ff".to_owned(),
            ModelId::new("probe-model")?,
        )
        .into_canonical(false)?;
        let restored = carried
            .segments()
            .get(0)
            .and_then(|segment| segment.items().first())
            .ok_or("carried history dropped the item")?;
        assert_eq!(restored.presentation_subject(), Some("c1,c2"));
        Ok(())
    }

    #[test]
    fn carry_without_replay_keeps_a_native_compactions_turn_record()
    -> Result<(), Box<dyn std::error::Error>> {
        let turns = SummarizedTurns::new(2, 3).ok_or("turns")?;
        let native = CompactionBlock::new("ciphertext")?
            .with_native_item("{\"type\":\"compaction\"}".to_owned())
            .with_summarized_turns(turns);
        let segment = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(8))?,
                Actor::Assistant,
                vec![ContentBlock::Compaction(native)],
                Provenance::ModelOutput,
            )?],
        )?;
        let carried = PortableSessionHistory::new(
            vec![segment],
            "00ff".to_owned(),
            ModelId::new("probe-model")?,
        )
        .into_canonical(false)?;
        let segment = carried
            .segments()
            .get(0)
            .ok_or("carried history dropped it")?;
        assert_eq!(segment.kind(), HistorySegmentKind::Compaction);
        let Some(ContentBlock::Compaction(block)) = segment
            .items()
            .first()
            .and_then(|item| item.blocks().first())
        else {
            return Err("the carried compaction is not a compaction block".into());
        };
        assert_eq!(block.native_item(), None);
        assert_eq!(block.summarized_turns(), Some(turns));
        Ok(())
    }

    /// A native successor is `[retained prefix..., summary]`, and later turns
    /// follow the summary. Cross-binding carry keeps the turn record on a plain
    /// compaction and moves that segment to position zero, ahead of the prefix.
    /// The marker stands in for context before those messages, which is what a
    /// semantic cut can fold.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one carried native history is checked for order, turn counts, and a sealed cut"
    )]
    fn carried_native_compaction_leads_and_still_admits_a_semantic_cut()
    -> Result<(), Box<dyn std::error::Error>> {
        use plakous_model::{
            CanonicalHistory, ContextHistoryPlan, ConversationView, SemanticCompactionCut,
            WorkingProjection,
        };

        let turns = SummarizedTurns::new(2, 3).ok_or("turns")?;
        let native = CompactionBlock::new("ciphertext")?
            .with_native_item("{\"type\":\"compaction\"}".to_owned())
            .with_summarized_turns(turns);
        let summary = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(2))?,
                Actor::Assistant,
                vec![ContentBlock::Compaction(native)],
                Provenance::ModelOutput,
            )?],
        )?;
        let prefix = prompt(1)?;
        let later = prompt(3)?;
        let reply = reply_segment(4)?;
        let source = vec![prefix, summary, later, reply];
        let source_counts = root_turn_counts(&source)?;

        let kept = PortableSessionHistory::new(
            source.clone(),
            "00ff".to_owned(),
            ModelId::new("probe-model")?,
        )
        .into_canonical(true)?;
        assert_eq!(
            kept.segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::Compaction,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::Conversation,
            ],
            "same-binding carry keeps the native summary last"
        );

        let carried =
            PortableSessionHistory::new(source, "00ff".to_owned(), ModelId::new("probe-model")?)
                .into_canonical(false)?;
        assert_eq!(
            carried
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::Compaction,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::Conversation,
            ]
        );
        let Some(ContentBlock::Compaction(block)) = carried
            .segments()
            .get(0)
            .and_then(|segment| segment.items().first())
            .and_then(|item| item.blocks().first())
        else {
            return Err("the leading segment is not a compaction block".into());
        };
        assert_eq!(block.native_item(), None);
        assert_eq!(block.summarized_turns(), Some(turns));
        assert_eq!(
            root_turn_counts(carried.segments().iter())?,
            source_counts,
            "leading the summary must keep (prompts, turns)"
        );
        assert_eq!(source_counts, (4, 5));

        let mut segments = carried.segments().iter().cloned().collect::<Vec<_>>();
        segments.extend([prompt(5)?, reply_segment(6)?, prompt(7)?, reply_segment(8)?]);
        let extended = root_turn_counts(&segments)?;
        assert_eq!(extended, (6, 7));
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments)?,
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source)?;
        let cut = SemanticCompactionCut::seal(&source, &plan).map_err(|error| {
            format!("carried native compaction must still admit a semantic cut: {error}")
        })?;
        assert!(cut.prior_compaction().is_some());
        let folded = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(9))?,
                Actor::Assistant,
                vec![ContentBlock::Compaction(CompactionBlock::new("folded")?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let successor = cut.build_successor(&folded)?;
        assert_eq!(
            successor.segments().get(0).map(HistorySegment::kind),
            Some(HistorySegmentKind::Compaction)
        );
        assert!(
            successor
                .segments()
                .iter()
                .skip(1)
                .all(|segment| segment.kind() != HistorySegmentKind::Compaction)
        );
        Ok(())
    }

    fn prompt(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
        said(
            n,
            Actor::User,
            Provenance::UserInput,
            HistorySegmentKind::RecentIntent,
        )
    }

    fn reply_segment(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
        said(
            n,
            Actor::Assistant,
            Provenance::ModelOutput,
            HistorySegmentKind::Conversation,
        )
    }

    fn said(
        n: u128,
        actor: Actor,
        provenance: Provenance,
        kind: HistorySegmentKind,
    ) -> Result<HistorySegment, Box<dyn std::error::Error>> {
        Ok(HistorySegment::new(
            kind,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(n))?,
                actor,
                vec![ContentBlock::Text(TextBlock::new(
                    format!("item {n}"),
                    None,
                )?)],
                provenance,
            )?],
        )?)
    }

    /// The structural turn count: a compaction record plus each `RecentIntent`
    /// user prompt. Order does not matter; a withheld-note tag is not involved.
    fn root_turn_counts<'a>(
        segments: impl IntoIterator<Item = &'a HistorySegment>,
    ) -> Result<(u64, u64), Box<dyn std::error::Error>> {
        let mut prompts = 0u64;
        let mut turns = 0u64;
        for segment in segments {
            match segment.kind() {
                HistorySegmentKind::Compaction => {
                    let Some(ContentBlock::Compaction(block)) = segment
                        .items()
                        .iter()
                        .flat_map(ConversationItem::blocks)
                        .find(|block| matches!(block, ContentBlock::Compaction(_)))
                    else {
                        return Err("compaction segment has no compaction block".into());
                    };
                    let record = block
                        .summarized_turns()
                        .ok_or("compaction without a turn record")?;
                    prompts = prompts.saturating_add(record.prompts());
                    turns = turns.saturating_add(record.turns());
                }
                HistorySegmentKind::RecentIntent
                    if segment.items().iter().any(|item| {
                        item.actor() == Actor::User && item.provenance() == Provenance::UserInput
                    }) =>
                {
                    prompts = prompts.saturating_add(1);
                    turns = turns.saturating_add(1);
                }
                _ => {}
            }
        }
        Ok((prompts, turns))
    }
}
