use crate::runtime::test_kit::TestExtension;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

use super::{CancelRefusal, CancelSettlement, InterruptWatch, InterruptWatchState, settle_cancel};
use crate::surface_protocol::user_action_channel;
use std::cell::RefCell;
use std::convert::Infallible;
use std::fmt;
use std::time::Duration;

/// A refusal that is not the runtime dropping the turn.
struct StillRunning;

impl fmt::Display for StillRunning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ingress closed")
    }
}

impl CancelRefusal for StillRunning {
    fn turn_is_no_longer_admitted(&self) -> bool {
        false
    }
}

/// The runtime's `Stale` refusal: the turn left the admitted set, which it
/// does only after publishing that turn's terminal.
struct TurnGone;

impl fmt::Display for TurnGone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("turn is no longer admitted")
    }
}

impl CancelRefusal for TurnGone {
    fn turn_is_no_longer_admitted(&self) -> bool {
        true
    }
}

impl CancelRefusal for Infallible {
    fn turn_is_no_longer_admitted(&self) -> bool {
        match *self {}
    }
}

#[tokio::test]
async fn cancel_that_never_settles_is_reported_unsettled() {
    let settlement = settle_cancel(
        std::future::pending::<Result<(), Infallible>>(),
        Duration::from_millis(30),
    )
    .await;
    let CancelSettlement::Unsettled(detail) = settlement else {
        panic!("a cancel that never settles must not be reported as settled");
    };
    assert!(detail.to_string().contains("did not settle"), "{detail}");
    assert!(detail.to_string().contains("still running"), "{detail}");
    assert!(detail.to_string().contains("30ms"), "{detail}");
}

#[tokio::test]
async fn cancel_error_is_reported_unsettled() {
    let settlement = settle_cancel(
        std::future::ready(Err::<(), _>(StillRunning)),
        Duration::from_secs(5),
    )
    .await;
    assert!(matches!(settlement, CancelSettlement::Unsettled(_)));
}

/// A quit whose cancel was refused because the runtime had
/// already published the turn's terminal reported "the turn is still
/// running", abandoned the turn, and blocked continuation.
#[tokio::test]
async fn cancel_refused_because_the_turn_is_gone_is_settled() {
    let settlement = settle_cancel(
        std::future::ready(Err::<(), _>(TurnGone)),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(settlement, CancelSettlement::Settled);
}

#[tokio::test]
async fn cancel_that_joins_is_settled() {
    let settlement = settle_cancel(
        std::future::ready(Ok::<_, Infallible>(())),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(settlement, CancelSettlement::Settled);
}

#[test]
fn watch_records_urgent_and_defers_ordinary_actions() {
    let (sender, receiver) = user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    sender.send(UserAction::Prompt("keep me".into())).unwrap();
    sender.send(UserAction::Interrupt).unwrap();
    watch.poll();
    assert!(watch.interrupted());
    let deferred = state.borrow_mut().take_deferred();
    assert_eq!(
        deferred.map(|delivery| delivery.into_parts().0),
        Some(UserAction::Prompt("keep me".into()))
    );
    assert!(state.borrow_mut().take_deferred().is_none());
}

#[test]
fn quit_is_deferred_to_the_host_loop_and_never_aborts_admitted_work() {
    let (sender, receiver) = user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    sender.send(UserAction::Quit).unwrap();
    watch.poll();
    assert!(!watch.interrupted());
    let deferred = state.borrow_mut().take_deferred();
    assert_eq!(
        deferred.map(|delivery| delivery.into_parts().0),
        Some(UserAction::Quit)
    );
}

/// The deferred lane is FIFO and is drained before the mailbox.
///
/// This is exactly what the drive loop does at the top of every iteration:
/// without it a deferred `Quit` (or an earlier `FollowUp`) is shadowed by a
/// whole turn, because the drive loop reads the mailbox directly.
#[test]
fn deferred_actions_precede_the_mailbox_and_keep_arrival_order() {
    let (sender, receiver) = user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    sender.send(UserAction::FollowUp("first".into())).unwrap();
    sender.send(UserAction::Quit).unwrap();
    watch.poll();

    // A later arrival goes to the mailbox, behind both deferred items.
    sender.send(UserAction::FollowUp("later".into())).unwrap();

    let mut observed = Vec::new();
    for _ in 0..3 {
        let delivery = state
            .borrow_mut()
            .take_deferred()
            .map_or_else(|| receiver.try_recv(), Ok)
            .expect("three actions are pending");
        observed.push(delivery.into_parts().0);
    }
    assert_eq!(
        observed,
        vec![
            UserAction::FollowUp("first".into()),
            UserAction::Quit,
            UserAction::FollowUp("later".into()),
        ]
    );
}

#[test]
fn begin_clears_a_previous_observation() {
    let (sender, receiver) = user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    sender.send(UserAction::Interrupt).unwrap();
    watch.poll();
    assert!(watch.interrupted());
    watch.begin();
    assert!(!watch.interrupted());
}
