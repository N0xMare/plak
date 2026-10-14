//! Bounded cancel settlement and mid-operation interrupt observation.
//!
//! Interrupt handling preserves two separate promises.
//!
//! * A cancel request is a *request*. The drive-loop Interrupt path submits
//!   `TurnControl::request_cancel` and observes settlement each iteration under
//!   [`plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT`]; it does not await
//!   [`settle_cancel`] on that branch. Quit and control-plane disconnect still
//!   join through [`settle_cancel`]. An unsettled cancel never becomes a
//!   terminal claim, and never borrows authority from an event.
//!
//! * Long host operations — compact, rebind, fork — must observe an
//!   `Interrupt` between provider round-trips. [`run_watched`] runs one
//!   round-trip on the host executor while the control mailbox keeps being
//!   polled, so the unresponsive window is one round-trip and not the whole
//!   operation.
//!
//! Every other action, `Quit` included, is *deferred* in exact arrival order
//! back to the host loop. Draining the mailbox to reach the urgent lane must
//! not consume ordinary product work, and `Quit` is documented by the control
//! mailbox as observed only after accepted ordinary work: an operation the
//! host already admitted runs to its own receipt and the loop then ends the
//! host. Cancelling a paid canonical operation midway is
//! forbids.
//!
//! That order holds across a turn boundary as well as inside the idle loop:
//! the drive loop takes from [`InterruptWatchState::take_deferred`] before it
//! reads the mailbox, so a deferred action that would otherwise be shadowed by
//! a whole turn — a `Quit` in particular — is still observed in the order it
//! arrived.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use crate::surface_protocol::{UserAction, UserActionDelivery, UserActionReceiver};
use plakous::agent::{ControlError, ControlErrorKind};
use plakous::application::{ApplicationExecutor, ApplicationExecutorError};

/// How often a watched provider round-trip yields to poll the control mailbox.
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Mailbox observations owned by the host loop for the life of one attach.
pub(crate) struct InterruptWatchState<P: plakous_host_protocol::ProtocolExtension> {
    interrupted: bool,
    deferred: VecDeque<UserActionDelivery<P>>,
}

impl<P: plakous_host_protocol::ProtocolExtension> Default for InterruptWatchState<P> {
    fn default() -> Self {
        Self {
            interrupted: false,
            deferred: VecDeque::new(),
        }
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> InterruptWatchState<P> {
    /// Pops the oldest action a watched operation drained but did not handle.
    pub(crate) fn take_deferred(&mut self) -> Option<UserActionDelivery<P>> {
        self.deferred.pop_front()
    }

    /// Whether any deferred action is still waiting to be handled.
    ///
    /// The drive loop asks before it starts a queued turn: an action the
    /// operator already sent — a `Quit`, an interrupt — must be answered before
    /// the queue opens a turn that would only have to be cancelled again.
    pub(crate) fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }
}

/// A borrowed handle threaded through one long host operation.
///
/// `Copy`, so it passes into nested handlers without borrow gymnastics.
#[derive(Clone)]
pub(crate) struct InterruptWatch<'a, P: plakous_host_protocol::ProtocolExtension> {
    actions: &'a UserActionReceiver<P>,
    state: &'a RefCell<InterruptWatchState<P>>,
}

impl<P: plakous_host_protocol::ProtocolExtension> Copy for InterruptWatch<'_, P> {}
impl<'a, P: plakous_host_protocol::ProtocolExtension> InterruptWatch<'a, P> {
    /// Binds one mailbox to the host-owned observation state.
    pub(crate) const fn new(
        actions: &'a UserActionReceiver<P>,
        state: &'a RefCell<InterruptWatchState<P>>,
    ) -> Self {
        Self { actions, state }
    }

    /// Clears the interrupt an earlier operation observed.
    ///
    /// Called once at the top of every long operation so a stale observation
    /// cannot abort the next one.
    pub(crate) fn begin(self) {
        self.state.borrow_mut().interrupted = false;
    }

    /// Drains the mailbox, recording the urgent lane and deferring the rest.
    pub(crate) fn poll(self) {
        while let Ok(delivery) = self.actions.try_recv() {
            let mut state = self.state.borrow_mut();
            if matches!(&*delivery, UserAction::Interrupt) {
                state.interrupted = true;
            } else {
                state.deferred.push_back(delivery);
            }
        }
    }

    /// Idle continuations must leave every action, including Escape, for the
    /// normal idle dispatcher rather than consume it as an operation cancel.
    pub(crate) fn defer_input(self) {
        while let Ok(delivery) = self.actions.try_recv() {
            self.state.borrow_mut().deferred.push_back(delivery);
        }
    }

    /// Whether an interrupt has been observed since [`Self::begin`].
    pub(crate) fn interrupted(self) -> bool {
        self.state.borrow().interrupted
    }

    /// Polls the mailbox first, then reports whether an interrupt is pending.
    ///
    /// Used at the checkpoints between a long operation's steps, so an intent
    /// that arrived while the host was doing local work is honoured too.
    pub(crate) fn observe_now(self) -> bool {
        self.poll();
        self.interrupted()
    }
}

/// Drives one provider round-trip while the control mailbox keeps being polled.
///
/// The round-trip itself is never cancelled: a paid canonical operation must
/// reach its own receipt so its outcome stays lawful. Only the
/// *next* step is skipped, by the caller, once [`InterruptWatch::interrupted`]
/// reports the intent.
pub(crate) fn run_watched<F, P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    future: F,
) -> Result<F::Output, ApplicationExecutorError>
where
    F: Future,
{
    executor.block_on(async move {
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                output = &mut future => return output,
                () = tokio::time::sleep(WATCH_POLL_INTERVAL) => watch.poll(),
            }
        }
    })
}

/// What the host actually observed about one cancel request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CancelSettlement {
    /// The runtime joined the cancel; `interrupted` may be asserted.
    Settled,
    /// The cancel timed out or was refused; the turn may still be running.
    Unsettled(String),
}

impl CancelSettlement {
    /// Whether the host observed the cancel settle.
    pub(crate) const fn settled(&self) -> bool {
        matches!(self, Self::Settled)
    }
}

/// One cancel refusal, read for the only distinction the host must draw:
/// whether the turn it addressed is still there to cancel.
pub(crate) trait CancelRefusal: fmt::Display {
    /// Whether the refusal says the runtime no longer holds the turn.
    ///
    /// The runtime drops a turn from its admitted set only after it has
    /// published that turn's terminal, so this refusal is an observation that
    /// the turn ended — the opposite of "the turn is still running". Live
    /// 2026-09-10: reporting it as unsettled made a quit abandon a turn whose
    /// terminal was already published, and the session fell to
    /// `terminal publication failed; continuation blocked`.
    fn turn_is_no_longer_admitted(&self) -> bool;
}

impl CancelRefusal for ControlError {
    fn turn_is_no_longer_admitted(&self) -> bool {
        self.kind() == ControlErrorKind::Stale
    }
}

/// Awaits one cancel request under a bounded timeout.
///
/// Generic over the cancel future so the timeout law is testable against a
/// runtime that never settles.
pub(crate) async fn settle_cancel<T, E, F>(cancel: F, timeout: Duration) -> CancelSettlement
where
    E: CancelRefusal,
    F: Future<Output = Result<T, E>>,
{
    match tokio::time::timeout(timeout, cancel).await {
        Ok(Ok(_)) => CancelSettlement::Settled,
        Ok(Err(error)) if error.turn_is_no_longer_admitted() => CancelSettlement::Settled,
        Ok(Err(error)) => CancelSettlement::Unsettled(format!(
            "cancel was refused by the runtime ({error}); the turn is still running"
        )),
        Err(_) => CancelSettlement::Unsettled(format!(
            "cancel did not settle within {}ms; the turn is still running",
            timeout.as_millis()
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;

    use super::{
        CancelRefusal, CancelSettlement, InterruptWatch, InterruptWatchState, settle_cancel,
    };
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
        assert!(detail.contains("did not settle"), "{detail}");
        assert!(detail.contains("still running"), "{detail}");
        assert!(detail.contains("30ms"), "{detail}");
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
}
