use super::{summarized_turns, turn_position};
use crate::runtime::driver::turn::{OPERATION_BUDGET_WRAP_UP_NOTE, output_limit_retry_note};
use plakous_model::{
    Actor, CanonicalHistory, CompactionBlock, ContentBlock, ConversationItem, EntryId,
    HistorySegment, HistorySegmentKind, Provenance, SummarizedTurns, TextBlock, TurnPosition,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn id(n: u128) -> Result<EntryId, Box<dyn std::error::Error>> {
    Ok(EntryId::try_from(uuid::Uuid::from_u128(n))?)
}

fn text(
    n: u128,
    actor: Actor,
    provenance: Provenance,
    body: &str,
) -> Result<ConversationItem, Box<dyn std::error::Error>> {
    Ok(ConversationItem::new(
        id(n)?,
        actor,
        vec![ContentBlock::Text(TextBlock::new(body, None)?)],
        provenance,
    )?)
}

fn segment(
    kind: HistorySegmentKind,
    items: Vec<ConversationItem>,
) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    Ok(HistorySegment::new(kind, items)?)
}

fn prompt(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    segment(
        HistorySegmentKind::RecentIntent,
        vec![text(n, Actor::User, Provenance::UserInput, "do the thing")?],
    )
}

fn wake(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    segment(
        HistorySegmentKind::RecentIntent,
        vec![text(
            n,
            Actor::User,
            Provenance::Instruction,
            "a child finished",
        )?],
    )
}

fn reply(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    segment(
        HistorySegmentKind::Conversation,
        vec![text(n, Actor::Assistant, Provenance::ModelOutput, "done")?],
    )
}

fn compaction(
    n: u128,
    turns: Option<SummarizedTurns>,
) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    let mut block = CompactionBlock::new("earlier turns")?;
    if let Some(turns) = turns {
        block = block.with_summarized_turns(turns);
    }
    segment(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            id(n)?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(block)],
            Provenance::ModelOutput,
        )?],
    )
}

fn history(segments: Vec<HistorySegment>) -> Result<CanonicalHistory, Box<dyn std::error::Error>> {
    Ok(CanonicalHistory::new(segments)?)
}

fn position(prompts: u64, turns: u64) -> Option<TurnPosition> {
    TurnPosition::new(prompts, turns)
}

#[test]
fn continuations_and_steering_stay_in_their_turn() -> TestResult {
    let steer = segment(
        HistorySegmentKind::Steering,
        vec![text(40, Actor::User, Provenance::UserInput, "also this")?],
    )?;
    let first = history(vec![prompt(1)?, reply(2)?])?;
    assert_eq!(turn_position(&first), position(1, 1));
    let second = history(vec![
        prompt(1)?,
        reply(2)?,
        prompt(3)?,
        reply(4)?,
        steer,
        reply(8)?,
    ])?;
    assert_eq!(turn_position(&second), position(2, 2));
    Ok(())
}

#[test]
fn wakes_count_as_turns_but_not_prompts_and_notes_count_as_neither() -> TestResult {
    // Round notes are conversation, as the driver appends them; their
    // text and provenance play no part.
    let wrap_up = segment(
        HistorySegmentKind::Conversation,
        vec![text(
            20,
            Actor::User,
            Provenance::UserInput,
            OPERATION_BUDGET_WRAP_UP_NOTE,
        )?],
    )?;
    let retry = segment(
        HistorySegmentKind::Conversation,
        vec![text(
            21,
            Actor::User,
            Provenance::UserInput,
            &output_limit_retry_note(None),
        )?],
    )?;
    let imported = segment(
        HistorySegmentKind::Conversation,
        vec![text(22, Actor::User, Provenance::Imported, "old")?],
    )?;
    let history = history(vec![
        prompt(1)?,
        wrap_up,
        retry,
        reply(2)?,
        wake(3)?,
        reply(4)?,
        imported,
        prompt(5)?,
    ])?;
    assert_eq!(turn_position(&history), position(2, 3));
    Ok(())
}

#[test]
fn a_compaction_record_carries_the_summarized_turns() -> TestResult {
    let source = history(vec![
        prompt(1)?,
        reply(2)?,
        wake(3)?,
        reply(4)?,
        prompt(5)?,
        reply(6)?,
    ])?;
    let kept = vec![prompt(5)?, reply(6)?];
    let turns = summarized_turns(&source, &kept).ok_or("tally")?;
    assert_eq!(SummarizedTurns::new(1, 2), Some(turns));
    let mut successor = vec![compaction(9, Some(turns))?];
    successor.extend(kept);
    let successor = history(successor)?;
    assert_eq!(turn_position(&successor), turn_position(&source));
    // A second compaction folds the first record into its own.
    let again = summarized_turns(&successor, &[]).ok_or("tally")?;
    assert_eq!(SummarizedTurns::new(2, 3), Some(again));
    let next = history(vec![compaction(10, Some(again))?, prompt(11)?])?;
    assert_eq!(turn_position(&next), position(3, 4));
    Ok(())
}

#[test]
fn a_fold_records_its_turns_on_the_published_summary() -> TestResult {
    let source = history(vec![
        prompt(1)?,
        reply(2)?,
        wake(3)?,
        reply(4)?,
        prompt(5)?,
        reply(6)?,
    ])?;
    let summary = compaction(9, None)?;
    let successor = history(vec![summary.clone(), prompt(5)?, reply(6)?])?;
    assert_eq!(turn_position(&successor), None);
    let (recorded, successor) =
        crate::runtime::compaction::fold::record_summarized_turns(&source, summary, successor)?;
    assert_eq!(successor.segments()[0].identity(), recorded.identity());
    assert_eq!(turn_position(&successor), position(2, 3));
    assert_eq!(turn_position(&successor), turn_position(&source));
    Ok(())
}

#[test]
fn an_unrecorded_compaction_leaves_the_position_unknown() -> TestResult {
    let history = history(vec![compaction(1, None)?, prompt(2)?])?;
    assert_eq!(turn_position(&history), None);
    assert_eq!(summarized_turns(&history, &[]), None);
    assert_eq!(turn_position(&CanonicalHistory::new(Vec::new())?), None);
    Ok(())
}
