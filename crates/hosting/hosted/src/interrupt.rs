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
//! host. Cancelling a paid canonical operation midway is forbidden.
//! Pre-turn preparation also uses this watch, but has not admitted a root
//! turn: its checkpoints stop for either Interrupt or deferred Quit before
//! compaction or root dispatch, after the current step reaches its receipt.
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

    /// Returns an earlier delivery held across restored-turn driving to the FIFO.
    pub(crate) fn restore_front(&mut self, delivery: UserActionDelivery<P>) {
        self.deferred.push_front(delivery);
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

    /// Preparation has no admitted operation to finish after its current
    /// receipt. Leave Quit in FIFO order, but never start a turn ahead of it.
    pub(crate) fn preparation_stopped(self) -> bool {
        self.poll();
        let state = self.state.borrow();
        state.interrupted
            || state
                .deferred
                .iter()
                .any(|delivery| matches!(&**delivery, UserAction::Quit))
            || self.actions.abandon_and_release_requested()
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
mod tests;
