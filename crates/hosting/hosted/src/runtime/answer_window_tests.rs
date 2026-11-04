//! A late addressed answer goes out inside its requester's `IntentStarted`
//! window (spec 02 §4.4): reopened for it, then the window it interrupted.

use plakous_host_protocol::wire::NodeAddress;
use plakous_host_protocol::{
    ContentObservation, Core, Draft, SessionIntent, intent_channel, observation_channel,
};

use super::Sender;
use crate::host_service::{HostService, Outbound};

type Observation = plakous_host_protocol::SessionObservation<Core>;

fn started(correlation: &str) -> ContentObservation<Core> {
    ContentObservation::IntentStarted(Some(correlation.into()))
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
            started("a"),
            restored("late"),
            started("b"),
            restored("now"),
        ]
        .map(Observation::from)
    );
}

/// An answer inside its own window, and anything not addressed, go out as
/// they are; a sender bound before any window answers as an unbound one.
#[test]
fn an_answer_in_its_own_window_or_not_addressed_goes_as_it_is() {
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
            restored("in time"),
            started("b"),
            ContentObservation::AssistantDone,
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
    let first = service.attach(&root).unwrap();
    let second = service.attach(&root).unwrap();
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
                Outbound::ResyncRequired => None,
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
            started("a"),
            restored("late"),
            ContentObservation::IntentStarted(None),
        ]
        .map(Observation::from)
    );
}
