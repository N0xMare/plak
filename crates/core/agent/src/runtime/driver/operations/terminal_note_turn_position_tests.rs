//! A note the driver leaves after a cancelled, failed or withheld prompt,
//! or inside a running turn, is not a root turn of its own.
use super::{
    TerminalPromptTail, cancelled_prompt_note, failed_prompt_note, turn_note_entry_id,
    withheld_prompt_note,
};
use crate::runtime::driver::turn_position::turn_position;
use crate::{AgentError, AgentErrorKind};
use plakous_model::{
    Actor, CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment,
    HistorySegmentKind, Provenance, TextBlock, TurnId, TurnPosition,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn turn(n: u128) -> Result<TurnId, Box<dyn std::error::Error>> {
    Ok(TurnId::try_from(uuid::Uuid::from_u128(0x7000 + n))?)
}

fn text(
    n: u128,
    actor: Actor,
    provenance: Provenance,
    body: &str,
) -> Result<ConversationItem, Box<dyn std::error::Error>> {
    Ok(ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(n))?,
        actor,
        vec![ContentBlock::Text(TextBlock::new(body, None)?)],
        provenance,
    )?)
}

fn opening(n: u128, provenance: Provenance) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    Ok(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(n, Actor::User, provenance, "do the thing")?],
    )?)
}

fn prompt(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    opening(n, Provenance::UserInput)
}

fn reply(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    Ok(HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(n, Actor::Assistant, Provenance::ModelOutput, "done")?],
    )?)
}

/// `history` with the terminal commit of `tail` applied, exactly as
/// `fail_turn` publishes it.
fn terminal(
    mut segments: Vec<HistorySegment>,
    tail: TerminalPromptTail,
) -> Result<Vec<HistorySegment>, Box<dyn std::error::Error>> {
    let (withhold_prompt, segment) = tail.into_commit()?;
    if withhold_prompt {
        segments.pop();
    }
    segments.extend(segment);
    Ok(segments)
}

#[test]
fn a_cancelled_turn_note_does_not_open_a_turn() -> TestResult {
    let mut segments = terminal(
        vec![prompt(1)?, reply(2)?, prompt(3)?],
        TerminalPromptTail::Note(cancelled_prompt_note(turn(2)?)?),
    )?;
    segments.push(prompt(5)?);
    // Three typed prompts, three root turns.
    assert_eq!(
        turn_position(&CanonicalHistory::new(segments)?),
        TurnPosition::new(3, 3)
    );
    Ok(())
}

#[test]
fn a_failed_turn_note_does_not_open_a_turn() -> TestResult {
    let error = AgentError::new(AgentErrorKind::Internal, "failed");
    let mut segments = terminal(
        vec![prompt(1)?],
        TerminalPromptTail::Note(failed_prompt_note(turn(1)?, &error)?),
    )?;
    segments.push(prompt(5)?);
    assert_eq!(
        turn_position(&CanonicalHistory::new(segments)?),
        TurnPosition::new(2, 2)
    );
    Ok(())
}

#[test]
fn a_withheld_prompt_still_counts_as_a_typed_prompt() -> TestResult {
    // The note replaces the prompt; the person still submitted it.
    let mut segments = terminal(
        vec![prompt(1)?, reply(2)?, prompt(3)?],
        TerminalPromptTail::WithholdPrompt(withheld_prompt_note(turn(2)?, 4096, true)?),
    )?;
    let note = segments.last().ok_or("note")?;
    assert_eq!(note.kind(), HistorySegmentKind::RecentIntent);
    assert_eq!(note.items()[0].provenance(), Provenance::Instruction);
    segments.push(prompt(5)?);
    assert_eq!(
        turn_position(&CanonicalHistory::new(segments)?),
        TurnPosition::new(3, 3)
    );
    Ok(())
}

#[test]
fn a_withheld_wake_still_counts_as_a_wake() -> TestResult {
    let mut segments = terminal(
        vec![prompt(1)?, reply(2)?, opening(3, Provenance::Instruction)?],
        TerminalPromptTail::WithholdPrompt(withheld_prompt_note(turn(2)?, 4096, false)?),
    )?;
    segments.push(prompt(5)?);
    assert_eq!(
        turn_position(&CanonicalHistory::new(segments)?),
        TurnPosition::new(2, 3)
    );
    Ok(())
}

#[test]
fn content_filter_guidance_stays_in_its_turn() -> TestResult {
    // Exactly the item `retry_content_filtered_incomplete` appends, with
    // the kind its `None` resolves to.
    let entry =
        turn_note_entry_id(turn(1)?, b"plakous-agent/content-filter-guidance-v1").ok_or("entry")?;
    let guidance = ConversationItem::new(
        entry,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("rephrase", None)?)],
        Provenance::Instruction,
    )?;
    let history = CanonicalHistory::new(vec![
        prompt(1)?,
        HistorySegment::new(
            crate::runtime::checkpoint::identity::history_segment_kind(std::slice::from_ref(
                &guidance,
            )),
            vec![guidance],
        )?,
    ])?;
    // The retry round of the first turn is still turn 1.
    assert_eq!(turn_position(&history), TurnPosition::new(1, 1));
    Ok(())
}
