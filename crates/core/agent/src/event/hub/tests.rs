#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert directly"
)]

use plakous_model::TextBlock;
use uuid::Uuid;

use super::*;
use crate::event::hub::subscription::*;
use crate::event::projection::*;
use crate::event::test_support::*;
use crate::event::types::*;
use crate::{AgentErrorKind, TurnUsage};
use plakous_model::{Actor, ContentBlock, ConversationItem, EntryId, Provenance};

#[test]
fn canonical_conversation_seeds_fresh_snapshot_without_events() -> TestResult {
    let conversation = ConversationView::from_items(vec![ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(9))?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "seeded checkpoint",
            None,
        )?)],
        Provenance::UserInput,
    )?])?;
    let fixture = fixture_with_run_and_conversation(4, &conversation, 8, 64 * 1024)?;

    let content = fixture.hub.attach(ViewRequest::content())?;
    assert_eq!(
        content.snapshot().cursor().run_id(),
        RunId::try_from(Uuid::from_u128(4))?
    );
    assert_eq!(
        content.snapshot().cursor().sequence(),
        EventSequence::INITIAL
    );
    assert_eq!(content.snapshot().committed_conversation_items(), 1);
    assert_eq!(
        content
            .snapshot()
            .conversation()
            .and_then(|items| items.first())
            .and_then(ConversationItemView::display_text),
        Some("seeded checkpoint")
    );

    let metadata = fixture.hub.attach(ViewRequest::metadata())?;
    assert_eq!(metadata.snapshot().committed_conversation_items(), 1);
    assert!(metadata.snapshot().conversation().is_none());
    Ok(())
}

#[test]
fn long_conversation_storage_is_shared_across_events_and_snapshots() -> TestResult {
    let conversation = long_conversation(2_048)?;
    let fixture = fixture_with_run_and_conversation(4, &conversation, 8, 4 * 1024 * 1024)?;
    let before = fixture.hub.attach(ViewRequest::content())?;
    let before_items = before
        .snapshot()
        .conversation()
        .ok_or("content snapshot omitted conversation")?;
    assert_eq!(before.snapshot().committed_conversation_items(), 2_048);
    assert_eq!(before_items.len(), 8);

    fixture.hub.publish(
        EventContext::turn(fixture.turn_id),
        AgentEventKind::TurnStarted,
    )?;
    let after = fixture.hub.attach(ViewRequest::content())?;
    let after_items = after
        .snapshot()
        .conversation()
        .ok_or("content snapshot omitted conversation")?;

    assert_eq!(before_items, after_items);
    assert!(std::ptr::eq(before_items.as_ptr(), after_items.as_ptr()));
    Ok(())
}

#[test]
fn rejected_event_preserves_projection_indexes_and_cursor_exactly() -> TestResult {
    let fixture = fixture(16, 256 * 1024)?;
    publish_started(&fixture)?;
    let before_snapshot = fixture.hub.attach(ViewRequest::metadata())?.snapshot;
    let before = {
        let state = lock(&fixture.hub.inner.state);
        (
            (
                state.branch_sequence,
                state.last_timestamp,
                state.retained.clone(),
                state.retained_bytes,
                state.closed,
                state.turn_parents.clone(),
                state.attempt_parents.clone(),
            ),
            (
                state.call_parents.clone(),
                state.completed_turn_parents.clone(),
                state.turn_evicted_through.clone(),
                state.turn_subscribers.clone(),
                state.completed_turns.clone(),
                state.last_event,
            ),
        )
    };

    assert_eq!(
        fixture.hub.publish(
            EventContext::turn(fixture.turn_id),
            AgentEventKind::TurnStarted,
        ),
        Err(ViewError::InvalidEvent)
    );

    let after_snapshot = fixture.hub.attach(ViewRequest::metadata())?.snapshot;
    let after = {
        let state = lock(&fixture.hub.inner.state);
        (
            (
                state.branch_sequence,
                state.last_timestamp,
                state.retained.clone(),
                state.retained_bytes,
                state.closed,
                state.turn_parents.clone(),
                state.attempt_parents.clone(),
            ),
            (
                state.call_parents.clone(),
                state.completed_turn_parents.clone(),
                state.turn_evicted_through.clone(),
                state.turn_subscribers.clone(),
                state.completed_turns.clone(),
                state.last_event,
            ),
        )
    };
    assert_eq!(after_snapshot, before_snapshot);
    assert_eq!(after, before);
    Ok(())
}

#[test]
fn predecessor_epoch_cursor_is_rejected_after_reopen() -> TestResult {
    let predecessor =
        fixture_with_run_and_conversation(4, &ConversationView::default(), 8, 64 * 1024)?;
    let cursor = predecessor
        .hub
        .attach(ViewRequest::metadata())?
        .snapshot()
        .cursor();
    let reopened =
        fixture_with_run_and_conversation(44, &ConversationView::default(), 8, 64 * 1024)?;

    assert_eq!(cursor.session_id(), reopened.hub.inner.session_id);
    assert_eq!(cursor.branch_id(), reopened.hub.inner.branch_id);
    assert_ne!(cursor.run_id(), reopened.hub.inner.run_id);
    assert!(matches!(
        reopened.hub.resume(cursor, ViewRequest::metadata()),
        Err(ViewError::IncompatibleCursor)
    ));
    Ok(())
}

#[tokio::test]
async fn retention_gap_requires_atomic_resnapshot() -> TestResult {
    let fixture = fixture(2, 64 * 1024)?;
    let request = ViewRequest::new(
        EventVisibility::Metadata,
        SubscriptionLimits::new(8, 64 * 1024)?,
    );
    let (initial, mut events) = fixture.hub.attach(request)?.into_parts();

    publish_started(&fixture)?;
    fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        delta(b'a'),
    )?;
    fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        delta(b'b'),
    )?;

    assert!(matches!(
        events.recv().await,
        Err(SubscriptionEnd::SubscriberLagged {
            last_delivered,
            next_available: EventSequence(3),
        }) if last_delivered == initial.cursor()
    ));

    let ResumeOutcome::RangeUnavailable(resync) = fixture.hub.resume(initial.cursor(), request)?
    else {
        return Err("evicted cursor unexpectedly replayed".into());
    };
    let fresh = fixture.hub.attach(request)?;
    assert_eq!(resync.snapshot(), fresh.snapshot());
    assert_eq!(resync.snapshot().cursor().sequence(), EventSequence(4));
    Ok(())
}

#[tokio::test]
async fn droppable_loss_advances_cursor_then_fresh_snapshot_resyncs() -> TestResult {
    let fixture = fixture(16, 64 * 1024)?;
    publish_started(&fixture)?;
    let request = ViewRequest::new(
        EventVisibility::Metadata,
        SubscriptionLimits::new(1, 64 * 1024)?,
    );
    let (snapshot, mut events) = fixture.hub.attach(request)?.into_parts();
    let mut reducer = AgentViewReducer::from_snapshot(snapshot);

    fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        delta(b'a'),
    )?;
    fixture.hub.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        delta(b'b'),
    )?;
    fixture.hub.publish(
        EventContext::turn(fixture.turn_id),
        AgentEventKind::TurnCompleted {
            usage: TurnUsage::default(),
        },
    )?;

    let lost = events.recv_delivery().await?;
    let EventDelivery::Lost(range) = &lost else {
        return Err("subscriber pressure did not produce an explicit loss range".into());
    };
    assert_eq!(range.first(), EventSequence(3));
    assert_eq!(range.through(), EventSequence(4));
    assert_eq!(range.kind(), EventLossKind::ModelDelta);
    reducer.apply_delivery(&lost)?;
    reducer.apply_delivery(&events.recv_delivery().await?)?;
    assert!(reducer.snapshot().presentation_incomplete());
    assert_eq!(reducer.snapshot().cursor().sequence(), EventSequence(5));
    assert_eq!(reducer.snapshot().state(), AgentViewState::Running);
    assert_eq!(
        reducer.snapshot().turns()[0].state(),
        TurnViewState::Completed
    );

    let resynced = fixture.hub.attach(request)?;
    assert!(!resynced.snapshot().presentation_incomplete());
    assert_eq!(resynced.snapshot().cursor(), reducer.snapshot().cursor());
    assert_eq!(resynced.snapshot().turns(), reducer.snapshot().turns());
    Ok(())
}

fn publish_terminal_projection(pipeline: &EventPipeline, fixture: &Fixture) -> TestResult {
    pipeline.publish(
        EventContext::turn(fixture.turn_id),
        AgentEventKind::TurnStarted,
    )?;
    pipeline.publish(
        EventContext::attempt(fixture.turn_id, fixture.attempt_id),
        AgentEventKind::ModelStarted,
    )?;
    pipeline.publish(
        EventContext::turn(fixture.turn_id),
        AgentEventKind::TurnCompleted {
            usage: TurnUsage::default(),
        },
    )?;
    Ok(())
}

#[tokio::test]
async fn dropped_subscriber_does_not_change_terminal_projection() -> TestResult {
    let fixture = fixture(8, 64 * 1024)?;
    let pipeline = EventPipeline::new(fixture.hub.clone());
    let attachment = fixture.hub.attach(ViewRequest::content())?;
    drop(attachment);

    publish_terminal_projection(&pipeline, &fixture)?;
    let snapshot = fixture.hub.attach(ViewRequest::content())?;
    assert_eq!(
        snapshot.snapshot().turns()[0].state(),
        TurnViewState::Completed
    );
    assert_eq!(snapshot.snapshot().cursor().sequence(), EventSequence(3));
    Ok(())
}

#[test]
fn retained_suffix_starts_at_the_requested_sequence_after_eviction() -> TestResult {
    let fixture = fixture(4, 256 * 1024)?;
    publish_started(&fixture)?;
    let context = EventContext::attempt(fixture.turn_id, fixture.attempt_id);
    for byte in 0..6 {
        fixture.hub.publish(context, delta(byte))?;
    }
    let state = lock(&fixture.hub.inner.state);
    let sequences = |from| {
        state
            .retained_from(from)
            .map(|event| event.branch_sequence.0)
            .collect::<Vec<_>>()
    };
    assert_eq!(sequences(0), [5, 6, 7, 8]);
    assert_eq!(sequences(6), [6, 7, 8]);
    assert_eq!(sequences(8), [8]);
    assert_eq!(sequences(9), [] as [u64; 0]);
    Ok(())
}

#[tokio::test]
async fn attachment_racing_publication_has_one_contiguous_snapshot_tail() -> TestResult {
    let fixture = fixture(16, 256 * 1024)?;
    let pipeline = EventPipeline::new(fixture.hub.clone());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let hub = fixture.hub.clone();
    let attach_barrier = Arc::clone(&barrier);
    let attaching = std::thread::spawn(move || {
        attach_barrier.wait();
        hub.attach(ViewRequest::metadata())
    });
    barrier.wait();
    pipeline.publish(
        EventContext::turn(fixture.turn_id),
        AgentEventKind::TurnStarted,
    )?;
    let (snapshot, mut events) = attaching
        .join()
        .expect("attachment thread panicked")?
        .into_parts();
    assert!(snapshot.cursor().sequence().get() <= 1);
    let mut reducer = AgentViewReducer::from_snapshot(snapshot);
    pipeline.close()?;
    loop {
        match events.recv().await {
            Ok(event) => reducer.apply(&event)?,
            Err(SubscriptionEnd::AgentClosed { last_delivered }) => {
                assert_eq!(last_delivered, reducer.snapshot().cursor());
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert_eq!(reducer.snapshot().cursor().sequence().get(), 2);
    assert_eq!(reducer.snapshot().state(), AgentViewState::Closed);
    assert_eq!(reducer.snapshot().turns().len(), 1);
    Ok(())
}

#[test]
fn publication_in_progress_linearizes_before_competing_close() -> TestResult {
    let fixture = fixture(16, 256 * 1024)?;
    let pipeline = EventPipeline::new(fixture.hub.clone());
    let publishing = pipeline.clone();
    let closing = pipeline.clone();
    let turn_id = fixture.turn_id;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let publication = std::thread::spawn(move || {
        publishing.publish_with(EventContext::turn(turn_id), |_| {
            entered_tx.send(()).expect("test observer disappeared");
            release_rx.recv().expect("test release disappeared");
            AgentEventKind::TurnStarted
        })
    });
    entered_rx.recv()?;
    let close = std::thread::spawn(move || closing.close());
    release_tx.send(())?;
    let published = publication.join().expect("publication thread panicked")?;
    close.join().expect("close thread panicked")?;
    assert_eq!(published.branch_sequence().get(), 1);
    let before = fixture.hub.attach(ViewRequest::metadata())?.into_parts().0;
    assert_eq!(before.cursor().sequence().get(), 2);
    assert_eq!(before.state(), AgentViewState::Closed);
    assert_eq!(
        pipeline
            .publish(EventContext::turn(turn_id), AgentEventKind::TurnStarted)
            .expect_err("closed pipeline accepted publication")
            .kind(),
        AgentErrorKind::Shutdown
    );
    pipeline.close()?;
    assert_eq!(
        fixture.hub.attach(ViewRequest::metadata())?.into_parts().0,
        before
    );
    Ok(())
}
