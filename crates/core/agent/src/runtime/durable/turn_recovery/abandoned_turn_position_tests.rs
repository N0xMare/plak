//! The abandonment note recovery leaves after an interrupted turn's
//! prompt is not a root turn of its own.
use super::{AbandonedTurnClaim, AbandonmentCause, abandoned_turn_segment};
use crate::runtime::driver::turn_position::turn_position;
use plakous_model::{
    Actor, CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment,
    HistorySegmentKind, Provenance, TextBlock, TurnId, TurnPosition,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn prompt(n: u128) -> Result<HistorySegment, Box<dyn std::error::Error>> {
    Ok(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(n))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("do the thing", None)?)],
            Provenance::UserInput,
        )?],
    )?)
}

#[test]
fn an_abandoned_turn_note_does_not_open_a_turn() -> TestResult {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x5eed))?;
    let history = CanonicalHistory::new(vec![
        prompt(1)?,
        abandoned_turn_segment(
            turn,
            AbandonedTurnClaim::ModelRequest,
            AbandonmentCause::ProcessLost,
        )?,
        prompt(3)?,
    ])?;
    assert_eq!(turn_position(&history), TurnPosition::new(2, 2));
    Ok(())
}
