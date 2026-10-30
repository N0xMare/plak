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
fn carry_without_replay_keeps_the_presentation_subject() -> Result<(), Box<dyn std::error::Error>> {
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
#[test]
fn opaque_tool_result_parts_strip_without_breaking_the_exchange()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous_model::{
        CallId, DataClassification, OpaqueBlock, OpaqueFallback, ProtocolId, ReplayEnvelope,
        ReplayKindId, ReplayScope, ToolCallBlock, ToolCallState, ToolCorrelation,
        ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind, ToolResultPart,
    };
    for fallback in [
        OpaqueFallback::Omit,
        OpaqueFallback::CompatibleReplay,
        OpaqueFallback::SemanticSummary("portable result".to_owned()),
    ] {
        let envelope = ReplayEnvelope::new(
            ProtocolId::new("source.protocol")?,
            None,
            ReplayScope::Protocol,
            ReplayKindId::new("opaque.result")?,
            1,
            DataClassification::Sensitive,
            serde_json::value::RawValue::from_string("\"sanitized-secret-replay\"".to_owned())?,
        )?;
        let id = CallId::try_from(uuid::Uuid::from_u128(1))?;
        let call = ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(2))?,
            Actor::Assistant,
            vec![ContentBlock::ToolCall(ToolCallBlock::new(
                id,
                ToolName::new("read")?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                "{}",
            )?)],
            Provenance::ModelOutput,
        )?;
        let result = ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(3))?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                id,
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Opaque(OpaqueBlock::new(
                    envelope,
                    fallback.clone(),
                )?)],
                ToolCorrelation::default(),
            )?)],
            Provenance::ToolExecution,
        )?;
        let segments = vec![HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![call, result],
        )?];
        let history = PortableSessionHistory::new(
            segments.clone(),
            "00ff".to_owned(),
            ModelId::new("source.model")?,
        );
        assert_eq!(
            history.clone().into_canonical(true)?.segments().to_vec(),
            segments
        );
        let stripped = history.into_canonical(false)?;
        let ContentBlock::ToolResult(result) =
            &stripped.segments().get(0).ok_or("missing segment")?.items()[1].blocks()[0]
        else {
            return Err("missing result".into());
        };
        assert_eq!(result.parts().len(), 1);
        let ToolResultPart::Text(text) = &result.parts()[0] else {
            return Err("opaque result survived".into());
        };
        assert_eq!(
            text.text(),
            if matches!(fallback, OpaqueFallback::SemanticSummary(_)) {
                "portable result"
            } else {
                "[provider replay omitted at carry boundary]"
            }
        );
        assert!(!serde_json::to_string(&stripped)?.contains("sanitized-secret-replay"));
    }
    Ok(())
}
