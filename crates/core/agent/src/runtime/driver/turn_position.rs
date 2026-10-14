//! A root turn's position in its logical session, read back from canonical
//! history.
//!
//! The position is two 1-based counts through the newest root turn: every
//! root turn, and only the turns a person submitted. Both survive resume and
//! compaction. Plakous keeps no counter: the turns still in history are
//! counted, and a compaction segment records the turns it removed, so reopen,
//! fork and compaction all read the same numbers.
//!
//! A root turn is recognised by what the driver appended to open it, never by
//! reading text: admission appends the turn's prompt (or a host wake's
//! instruction) as the one recent-intent segment of the turn. Everything else
//! the driver adds to a turn (round notes, terminal notes, guidance, job
//! notices, steering) is another segment kind.

use plakous_model::{
    Actor, CanonicalHistory, ContentBlock, ConversationItem, HistorySegment, HistorySegmentKind,
    Provenance, SummarizedTurns, TurnPosition,
};

use super::operations::is_withheld_prompt_entry;

/// Prompts and turns counted over some segments.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Tally {
    prompts: u64,
    turns: u64,
}

/// What a segment contributes to its history's turn count.
enum Contribution {
    None,
    Turn {
        person: bool,
    },
    Summarized(SummarizedTurns),
    /// A compaction that did not record the turns it removed.
    Unknown,
}

fn contribution(segment: &HistorySegment) -> Contribution {
    match segment.kind() {
        HistorySegmentKind::Compaction => segment
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .find_map(|block| match block {
                ContentBlock::Compaction(compaction) => Some(compaction.summarized_turns()),
                _ => None,
            })
            .flatten()
            .map_or(Contribution::Unknown, Contribution::Summarized),
        HistorySegmentKind::RecentIntent => segment
            .items()
            .iter()
            .find(|item| item.actor() == Actor::User)
            .map_or(Contribution::None, |item| match item.provenance() {
                Provenance::UserInput => Contribution::Turn { person: true },
                // A host wake, or the note standing where a person's
                // withheld prompt was.
                Provenance::Instruction => Contribution::Turn {
                    person: is_withheld_prompt_entry(item.id()),
                },
                _ => Contribution::None,
            }),
        _ => Contribution::None,
    }
}

/// Counts root turns over `segments`, compaction records included.
///
/// `None` when a compaction did not record what it removed: the position is
/// then unknown.
fn tally<'a>(segments: impl IntoIterator<Item = &'a HistorySegment>) -> Option<Tally> {
    let mut tally = Tally::default();
    for segment in segments {
        match contribution(segment) {
            Contribution::None => {}
            Contribution::Turn { person } => {
                tally.turns = tally.turns.saturating_add(1);
                tally.prompts = tally.prompts.saturating_add(u64::from(person));
            }
            Contribution::Summarized(turns) => {
                tally.turns = tally.turns.saturating_add(turns.turns());
                tally.prompts = tally.prompts.saturating_add(turns.prompts());
            }
            Contribution::Unknown => return None,
        }
    }
    Some(tally)
}

/// The position of the newest root turn in `history`.
pub(in crate::runtime) fn turn_position(history: &CanonicalHistory) -> Option<TurnPosition> {
    let tally = tally(history.segments().iter())?;
    TurnPosition::new(tally.prompts, tally.turns)
}

/// The root turns a successor no longer holds outside its compaction segment.
///
/// `successor` is every segment the compaction keeps or adds; its own
/// compaction segments are skipped, so the result is what the new compaction
/// segment must record for the successor to keep `source`'s position.
pub(in crate::runtime) fn summarized_turns<'a>(
    source: &CanonicalHistory,
    successor: impl IntoIterator<Item = &'a HistorySegment>,
) -> Option<SummarizedTurns> {
    let total = tally(source.segments().iter())?;
    let kept = tally(
        successor
            .into_iter()
            .filter(|segment| segment.kind() != HistorySegmentKind::Compaction),
    )?;
    SummarizedTurns::new(
        total.prompts.checked_sub(kept.prompts)?,
        total.turns.checked_sub(kept.turns)?,
    )
}

#[cfg(test)]
mod tests {
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

    fn history(
        segments: Vec<HistorySegment>,
    ) -> Result<CanonicalHistory, Box<dyn std::error::Error>> {
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
}
