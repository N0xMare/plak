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
mod tests;
