//! Shared observation fixtures, compiled only for tests.
use super::*;
use plakous_model::{
    Actor, AgentId, AttemptId, BranchId, ContentBlock, ConversationItem, ConversationView, EntryId,
    EventId, FixedClock, ModelEvent, OutputLocation, Provenance, RunId, SequenceIdGenerator,
    SessionId, TextBlock, TextDelta, Timestamp, TurnId,
};
use std::{error::Error, sync::Arc};
use uuid::Uuid;
pub(super) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

pub(super) struct Fixture {
    pub(super) hub: EventHub,
    pub(super) turn_id: TurnId,
    pub(super) attempt_id: AttemptId,
}

pub(super) fn fixture(retained_items: usize, retained_bytes: usize) -> TestResult<Fixture> {
    fixture_with_run_and_conversation(
        4,
        &ConversationView::default(),
        retained_items,
        retained_bytes,
    )
}

pub(super) fn fixture_with_run_and_conversation(
    run: u128,
    conversation: &ConversationView,
    retained_items: usize,
    retained_bytes: usize,
) -> TestResult<Fixture> {
    let timestamp = Timestamp::new(42, 7)?;
    let event_ids = Arc::new(SequenceIdGenerator::new(
        (100_u128..300).map(Uuid::from_u128),
    )?);
    let close_event_id = EventId::generate(&FixedClock::new(timestamp), event_ids.as_ref())?;
    let hub = EventHub::new(
        AgentId::try_from(Uuid::from_u128(1))?,
        SessionId::try_from(Uuid::from_u128(2))?,
        BranchId::try_from(Uuid::from_u128(3))?,
        RunId::try_from(Uuid::from_u128(run))?,
        Arc::new(FixedClock::new(timestamp)),
        event_ids,
        close_event_id,
        conversation,
        EventHubLimits::new(retained_items, retained_bytes, 16),
    )?;
    Ok(Fixture {
        hub,
        turn_id: TurnId::try_from(Uuid::from_u128(5))?,
        attempt_id: AttemptId::try_from(Uuid::from_u128(6))?,
    })
}

pub(super) fn delta(byte: u8) -> AgentEventKind {
    AgentEventKind::model(ModelEvent::TextDelta(TextDelta::new(
        OutputLocation::new(0, 0),
        vec![byte],
    )))
}

pub(super) fn publish_started(fixture: &Fixture) -> Result<(), ViewError> {
    fixture.hub.publish(
        EventContext::turn(fixture.turn_id),
        AgentEventKind::TurnStarted,
    )?;
    fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        AgentEventKind::ModelStarted,
    )?;
    Ok(())
}

pub(super) fn conversation_item(id: u128, text: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(id))?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None)?)],
        Provenance::UserInput,
    )?)
}

pub(super) fn long_conversation(item_count: u128) -> TestResult<ConversationView> {
    let items = (0..item_count)
        .map(|index| conversation_item(10_000 + index, &format!("history item {index}")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConversationView::from_items(items)?)
}
