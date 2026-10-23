//! The steering note a mid-turn fold leaves for the next model round.
//!
//! After a fold at the tool-group fence the model sees its whole earlier turn
//! as one assistant summary followed by the user's request and the current
//! round's batch. Live Grok runs ended the turn right there with narration
//! ("eof is still false, so the read continues at offset 221") and no tool
//! call: the newest assistant text it saw was a summary that describes the next
//! step instead of taking it. The note sits at the tail, where the model is
//! working, and says the task is still open.

use crate::runtime::PostCompactionNote;
use crate::runtime::compaction::placeholder::harness_note_segment;
use crate::runtime::compaction::record::compaction_uuid;
use crate::runtime::driver::is_process_notice_batch;
use crate::{AgentError, AgentErrorKind, SessionWorkflowId};
use plakous_model::{
    Actor, CanonicalHistory, ContentBlock, EntryId, HistorySegment, HistorySegmentKind, Provenance,
};

/// Opens the continuation note so a later fold in the same turn recognizes it.
pub(in crate::runtime) const TURN_CONTINUATION_NOTE_MARKER: &str =
    "[plak: earlier work in this turn was summarized]";

const TURN_CONTINUATION_NOTE_BODY: &str = "The earlier part of this turn is now summarized \
     above. The task is not finished: continue from the latest tool result with your next tool \
     call, and end the turn only when the task is done or you need the user.";

/// The history planner's notion of user intent: steering is not intent.
pub(in crate::runtime) fn is_user_intent(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::RecentIntent
        || (segment.kind() == HistorySegmentKind::Conversation
            && segment
                .items()
                .iter()
                .any(|item| item.actor() == Actor::User))
}

/// The text of a harness note: a steering segment shaped exactly as
/// [`harness_note_segment`] builds it, one `Instruction` user item holding one
/// text block. A person's steering carries `UserInput` and is never one.
fn harness_note_text(segment: &HistorySegment) -> Option<&str> {
    if segment.kind() != HistorySegmentKind::Steering {
        return None;
    }
    let [item] = segment.items() else {
        return None;
    };
    if item.actor() != Actor::User || item.provenance() != Provenance::Instruction {
        return None;
    }
    let [ContentBlock::Text(text)] = item.blocks() else {
        return None;
    };
    Some(text.text())
}

/// Whether a native successor leaves `segment` out of the groups it retains
/// ahead of its compaction item: a harness note such as the continuation,
/// plan restoration, placeholder or replay-omission note, which the server's
/// compaction item covers and which is not user intent. A process-notice
/// batch is kept: it carries facts delivered once (exit codes, output tails,
/// the unknown-outcome warning) that nothing appends again.
pub(in crate::runtime) fn is_covered_harness_note(segment: &HistorySegment) -> bool {
    harness_note_text(segment).is_some_and(|text| !is_process_notice_batch(text))
}

fn is_turn_continuation_note(segment: &HistorySegment) -> bool {
    harness_note_text(segment).is_some_and(|text| text.starts_with(TURN_CONTINUATION_NOTE_MARKER))
}

/// Appends the post-compaction note `note` derives from a fold of `source`
/// into `successor`, when it derives one. Live and recovery publication both
/// call this over the same source and successor, so a recovered fold carries
/// the note the live one would have.
pub(in crate::runtime) fn with_post_compaction_note(
    successor: CanonicalHistory,
    source: &CanonicalHistory,
    note: Option<&PostCompactionNote>,
    entry_id: impl FnOnce() -> Result<EntryId, AgentError>,
) -> Result<CanonicalHistory, AgentError> {
    let Some(text) = note.and_then(|derive| derive(source, &successor)) else {
        return Ok(successor);
    };
    successor
        .append(harness_note_segment(entry_id()?, text)?)
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "compaction note could not be appended",
            )
        })
}

/// Appends the continuation note to a fold successor whose newest
/// non-steering segment is a tool exchange: the fence-(b) shape, where the same
/// turn's next round follows. Trailing steering (the plan restoration note)
/// does not change that shape.
///
/// A successor ending anywhere else (a fold before a new prompt) is returned
/// unchanged, and so is one whose current turn already carries a note. The
/// compaction provenance pins the successor prefix, so a fold may only append.
/// A semantic fold never folds steering inside its own turn: an earlier fold's
/// note stays, and after the next fold it sits just before the current round's
/// batch, still in front of the model. Appending one per fold would stack them.
///
/// A native successor that keeps the current round's batch after its
/// compaction item gets the note after that tail; one that ends in its
/// compaction item does not. A native successor retains no harness note ahead
/// of its item, so a repeated native compaction in one turn gets a fresh note
/// after its tail.
pub(in crate::runtime) fn with_turn_continuation_note(
    successor: CanonicalHistory,
    workflow_id: SessionWorkflowId,
) -> Result<CanonicalHistory, AgentError> {
    if successor
        .segments()
        .iter()
        .filter(|segment| segment.kind() != HistorySegmentKind::Steering)
        .last()
        .is_none_or(|segment| segment.kind() != HistorySegmentKind::ToolExchange)
    {
        return Ok(successor);
    }
    let turn_start = successor
        .segments()
        .iter()
        .enumerate()
        .filter(|(_, segment)| is_user_intent(segment))
        .map(|(index, _)| index)
        .last()
        .unwrap_or(0);
    let turn_has_note = successor
        .segments()
        .iter()
        .skip(turn_start)
        .any(is_turn_continuation_note);
    if turn_has_note {
        return Ok(successor);
    }
    let entry_id = EntryId::try_from(compaction_uuid(workflow_id, b"turn-continuation-note")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    successor
        .append(harness_note_segment(
            entry_id,
            format!("{TURN_CONTINUATION_NOTE_MARKER}\n{TURN_CONTINUATION_NOTE_BODY}"),
        )?)
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "compaction continuation note could not be appended",
            )
        })
}

#[cfg(test)]
mod tests;
