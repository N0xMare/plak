//! A late addressed answer goes out inside its requester's `IntentStarted`
//! window (spec 02 §4.4): reopened for it, then the window it interrupted.

use plakous_host_protocol::wire::NodeAddress;
use plakous_host_protocol::{
    ContentObservation, Core, Draft, SessionIntent, intent_channel, observation_channel,
};

use super::Sender;
use crate::host_service::wire::Face;
use crate::host_service::{HostService, Outbound};

type Observation = plakous_host_protocol::SessionObservation<Core>;

fn started(correlation: &str) -> ContentObservation<Core> {
    ContentObservation::IntentStarted(Some(correlation.into()))
}

/// The marked start of the window holding only `correlation`'s answer.
fn answers(correlation: &str) -> ContentObservation<Core> {
    ContentObservation::IntentStarted(Some(format!(
        "{}{correlation}",
        crate::host_service::ANSWER
    )))
}

fn restored(text: &str) -> ContentObservation<Core> {
    ContentObservation::DraftRestored(Draft::from(text))
}

#[test]
fn a_late_answer_reopens_its_requesters_window_then_the_open_one() {
    let (channel, received) = observation_channel::<Core>();
    let events = Sender::over(channel);
    assert!(events.send(started("a")));
    let reader = events.answering();
    assert!(events.send(started("b")));
    assert!(reader.send(restored("late")));
    assert!(events.send(restored("now")));
    let seen: Vec<Observation> = received.try_iter().collect();
    assert_eq!(
        seen,
        [
            started("a"),
            started("b"),
            answers("a"),
            restored("late"),
            started("b"),
            restored("now"),
        ]
        .map(Observation::from)
    );
}

/// Whatever a bound sender sends is marked as its intent's answer, inside
/// that intent's own window too, so no other observation in the window is
/// taken for it; a sender bound before any window answers as an unbound one.
#[test]
fn whatever_a_bound_sender_sends_is_marked() {
    let (channel, received) = observation_channel::<Core>();
    let events = Sender::over(channel);
    let early = events.answering();
    assert!(events.send(started("a")));
    let reader = events.answering();
    assert!(reader.send(restored("in time")));
    assert!(events.send(started("b")));
    assert!(reader.send(ContentObservation::AssistantDone));
    assert!(early.send(restored("unbound")));
    let seen: Vec<Observation> = received.try_iter().collect();
    assert_eq!(
        seen,
        [
            started("a"),
            answers("a"),
            restored("in time"),
            started("a"),
            started("b"),
            answers("a"),
            ContentObservation::AssistantDone,
            started("b"),
            restored("unbound"),
        ]
        .map(Observation::from)
    );
}

/// Through the host service, the late answer reaches the attachment whose
/// intent it answers, and not the one whose window it arrived in.
#[test]
fn a_late_answer_reaches_its_requester_through_the_service() {
    let (actions, intents) = intent_channel::<Core>();
    let root = NodeAddress {
        tree_id: "t1".into(),
        agent_id: "t1".into(),
    };
    let service = HostService::new(actions, root.clone(), 64);
    let first = service.attach(&root, Face::Remote).unwrap();
    let second = service.attach(&root, Face::Remote).unwrap();
    while intents.try_recv().is_ok() {}
    let (channel, received) = observation_channel::<Core>();
    let events = Sender::over(channel);
    service
        .intent_correlated(
            first,
            SessionIntent::Prompt(Draft::from("read")),
            Some("r1"),
        )
        .unwrap();
    service
        .intent_correlated(
            second,
            SessionIntent::Prompt(Draft::from("next")),
            Some("r2"),
        )
        .unwrap();
    assert!(events.send(intents.try_recv().unwrap().started().unwrap()));
    let reader = events.answering();
    assert!(events.send(intents.try_recv().unwrap().started().unwrap()));
    assert!(reader.send(restored("late")));
    assert!(events.send(restored("now")));
    service.take(first);
    service.take(second);
    for observation in received.try_iter() {
        service.observe(observation);
    }
    let taken = |attachment| -> Vec<Observation> {
        service
            .take(attachment)
            .into_iter()
            .filter_map(|item| match item {
                Outbound::Event { observation, .. } => Some(observation),
                Outbound::ResyncRequired | Outbound::Moved { .. } => None,
            })
            .collect()
    };
    assert_eq!(
        taken(first),
        [started("r1"), started("r1"), restored("late")].map(Observation::from)
    );
    assert_eq!(
        taken(second),
        [started("r2"), started("r2"), restored("now")].map(Observation::from)
    );
}

/// A correlation reset closes the window: a late answer after it reopens
/// its requester's window, then leaves the session in no window rather
/// than reviving the one the reset closed.
#[test]
fn a_late_answer_after_a_reset_restores_no_window() {
    let (channel, received) = observation_channel::<Core>();
    let events = Sender::over(channel);
    assert!(events.send(started("a")));
    let reader = events.answering();
    assert!(events.send(started("b")));
    assert!(events.send(ContentObservation::PromptCorrelationReset));
    assert!(reader.send(restored("late")));
    let seen: Vec<Observation> = received.try_iter().collect();
    assert_eq!(
        seen,
        [
            started("a"),
            started("b"),
            ContentObservation::PromptCorrelationReset,
            answers("a"),
            restored("late"),
            ContentObservation::IntentStarted(None),
        ]
        .map(Observation::from)
    );
}

/// The session's last usage view is kept whoever sent it, an answering
/// sender included.
#[test]
fn a_marked_usage_view_is_still_the_sessions_last() {
    let (channel, _received) = observation_channel::<Core>();
    let events = Sender::over(channel);
    assert!(events.send(started("a")));
    let view = plakous_host_protocol::views::presentation::UsageLimitsView {
        verticals: Vec::new(),
        session: plakous_host_protocol::views::presentation::SessionLedgerView::default(),
        stale_age_seconds: Some(3),
    };
    assert!(
        events
            .answering()
            .send(ContentObservation::UsageLimits(view.clone()))
    );
    assert_eq!(events.usage(), Some(view));
}
