#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert directly"
)]
use super::*;
use crate::event::test_support::*;
use crate::event::{EventContext, EventScope, EventVisibility, ViewRequest};
#[test]
fn presentation_events_only_advance_the_attach_cursor() -> TestResult {
    let fixture = fixture(16, 256 * 1024)?;
    publish_started(&fixture)?;
    let snapshot = fixture.hub.attach(ViewRequest::metadata())?.into_parts().0;
    let before_turns = snapshot.turns().to_vec();
    let mut reducer = AgentViewReducer::from_snapshot(snapshot);
    let event = fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        delta(b'x'),
    )?;
    reducer.apply(&event.projected(EventVisibility::Metadata, EventScope::Agent))?;
    assert_eq!(reducer.snapshot().turns(), before_turns);
    assert_eq!(
        reducer.snapshot().cursor(),
        event
            .projected(EventVisibility::Metadata, EventScope::Agent)
            .cursor()
    );
    Ok(())
}

#[test]
fn rejected_reducer_event_preserves_snapshot_and_cursor_exactly() -> TestResult {
    let fixture = fixture(16, 256 * 1024)?;
    publish_started(&fixture)?;
    let snapshot = fixture.hub.attach(ViewRequest::metadata())?.into_parts().0;
    let mut reducer = AgentViewReducer::from_snapshot(snapshot);
    let event = fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        delta(b'x'),
    )?;
    let event = event.projected(EventVisibility::Metadata, EventScope::Agent);
    reducer.apply(&event)?;
    let before = reducer.snapshot().clone();
    assert_eq!(reducer.apply(&event), Err(ReducerError::NonContiguous));
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
