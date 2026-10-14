#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert directly"
)]
use super::*;
use crate::event::test_support::*;
use crate::event::{EventContext, EventScope, EventVisibility, ViewRequest};
use plakous_model::Usage;
use uuid::Uuid;
#[test]
fn metadata_reducer_delta_preserves_long_history_allocations() -> TestResult {
    let conversation = long_conversation(4_096)?;
    let fixture = fixture_with_run_and_conversation(4, &conversation, 16, 8 * 1024 * 1024)?;
    publish_started(&fixture)?;
    let mut snapshot = fixture.hub.attach(ViewRequest::metadata())?.into_parts().0;
    snapshot.retained_turns = 8_192;
    snapshot.turns.reserve(4_096);
    for value in 100_000_u128..104_096 {
        let mut turn = TurnView::running(TurnId::try_from(Uuid::from_u128(value))?);
        turn.state = TurnViewState::Completed;
        snapshot.turns.push(turn);
    }
    let turns_address = snapshot.turns.as_ptr();
    let turns_capacity = snapshot.turns.capacity();
    let mut reducer = AgentViewReducer::from_snapshot(snapshot);
    let event = fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        delta(b'x'),
    )?;

    reducer.apply(&event.projected(EventVisibility::Metadata, EventScope::Agent))?;

    assert_eq!(reducer.snapshot().turns.as_ptr(), turns_address);
    assert_eq!(reducer.snapshot().turns.capacity(), turns_capacity);
    assert!(reducer.snapshot().conversation().is_none());
    Ok(())
}
#[test]
fn rejected_reducer_event_preserves_snapshot_and_cursor_exactly() -> TestResult {
    let fixture = fixture(16, 256 * 1024)?;
    publish_started(&fixture)?;
    let mut snapshot = fixture.hub.attach(ViewRequest::metadata())?.into_parts().0;
    let maximal = TurnUsage::new(Usage::new(u64::MAX, 0, 0, 0, 0), 1, 1, 0)?;
    snapshot.usage.record_turn(maximal)?;
    let terminal = fixture.hub.publish(
        EventContext::turn(fixture.turn_id),
        AgentEventKind::TurnCompleted { usage: maximal },
    )?;
    let before = snapshot.clone();
    let mut reducer = AgentViewReducer::from_snapshot(snapshot);

    assert_eq!(
        reducer.apply(&terminal.projected(EventVisibility::Metadata, EventScope::Agent)),
        Err(ReducerError::UsageOverflow)
    );
    assert_eq!(reducer.snapshot(), &before);
    Ok(())
}

/// The re-derivation note has to ride a turn, because a turn subscription is
/// the only one the product has.
///
/// Published agent-scoped it was accepted by the hub and dropped by every
/// subscriber, so the session log, the operator line and all three faces stayed
/// silent and a re-derived output bound was applied without saying so. This
/// pins the scope that makes it deliverable.
#[tokio::test]
async fn a_rederived_output_bound_rides_a_turn_and_reaches_a_turn_subscriber() -> TestResult {
    let fixture = fixture(16, 8 * 1024 * 1024)?;
    publish_started(&fixture)?;
    let mut subscriber = fixture
        .hub
        .turn_events(fixture.turn_id, crate::event::SubscriptionLimits::default())?;
    let kind = || AgentEventKind::OutputBoundRederived {
        previous: std::num::NonZeroU32::new(4_096),
        current: std::num::NonZeroU32::new(32_768),
    };
    // Agent-scoped is refused outright: the shape contract now says this kind
    // belongs to a turn.
    assert!(fixture.hub.publish(EventContext::agent(), kind()).is_err());

    let published = fixture
        .hub
        .publish(EventContext::turn(fixture.turn_id), kind())?;
    assert_eq!(published.turn_id(), Some(fixture.turn_id));
    let mut delivered = false;
    for _ in 0..8 {
        let event = subscriber.recv().await?;
        if matches!(
            event.kind(),
            AgentEventKind::OutputBoundRederived { previous, current }
                if previous.map(std::num::NonZeroU32::get) == Some(4_096)
                    && current.map(std::num::NonZeroU32::get) == Some(32_768)
        ) {
            delivered = true;
            break;
        }
    }
    assert!(delivered, "a turn subscriber never saw the re-derivation");
    Ok(())
}

#[test]
fn attachment_seeds_live_usage_without_double_counting_snapshots() -> TestResult {
    let fixture = fixture(16, 256 * 1024)?;
    publish_started(&fixture)?;
    let context = EventContext::attempt(fixture.turn_id, fixture.attempt_id);
    fixture
        .hub
        .publish(context, AgentEventKind::ModelDispatchStarted)?;
    for output in [2, 3] {
        fixture.hub.publish(
            context,
            AgentEventKind::model(ModelEvent::Usage(plakous_model::UsageUpdate::new(
                Usage::new(11, output, 1, 7, 0),
            ))),
        )?;
    }
    let snapshot = fixture.hub.attach(ViewRequest::content())?.into_parts().0;
    assert_eq!(
        snapshot.turns()[0].live_usage(),
        (Usage::new(11, 3, 1, 7, 0), Usage::default())
    );
    fixture
        .hub
        .publish(context, AgentEventKind::ModelDispatchStarted)?;
    fixture.hub.publish(
        context,
        AgentEventKind::model(ModelEvent::Usage(plakous_model::UsageUpdate::new(
            Usage::new(17, 5, 2, 9, 0),
        ))),
    )?;
    let snapshot = fixture.hub.attach(ViewRequest::content())?.into_parts().0;
    assert_eq!(
        snapshot.turns()[0].live_usage(),
        (Usage::new(28, 8, 3, 16, 0), Usage::new(11, 3, 1, 7, 0))
    );
    Ok(())
}
