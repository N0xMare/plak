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
//!   `Interrupt` between provider round-trips. [`watched`] awaits one
//!   round-trip inline in the host's actor while the control mailbox is
//!   drained as intents arrive, so the unresponsive window is one round-trip
//!   and not the whole operation.
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

use plakous_host_protocol::status::{CancelUnsettled, ControlFailure, DiagnosticText};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use crate::intent_dispatch::{DispatchPhase, Disposition, IntentClass};
use crate::surface_protocol::{UserActionDelivery, UserActionReceiver};
use plakous::agent::{ControlError, ControlErrorKind};

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

    /// Whether a deferred `Quit` is waiting to be handled.
    pub(crate) fn has_deferred_quit(&self) -> bool {
        self.deferred
            .iter()
            .any(|delivery| IntentClass::of(&**delivery) == IntentClass::Quit)
    }
}

/// Defers an urgent `item` behind the urgent actions already deferred and
/// ahead of everything else: the urgent lane still outranks queued work, and
/// urgent actions keep their arrival order among themselves.
fn defer_urgent<T>(deferred: &mut VecDeque<T>, item: T, urgent: impl Fn(&T) -> bool) {
    let at = deferred.iter().take_while(|queued| urgent(queued)).count();
    deferred.insert(at, item);
}

/// A borrowed handle threaded through one long host operation.
///
/// `Copy`, so it passes into nested handlers without borrow gymnastics.
#[derive(Clone)]
pub(crate) struct InterruptWatch<'a, P: plakous_host_protocol::ProtocolExtension> {
    actions: &'a UserActionReceiver<P>,
    state: &'a RefCell<InterruptWatchState<P>>,
    /// Idle-entry work has no next step an interrupt could skip: the urgent
    /// lane is deferred too, ahead of the rest, for the idle dispatcher.
    defer_urgent: bool,
}

impl<P: plakous_host_protocol::ProtocolExtension> Copy for InterruptWatch<'_, P> {}
impl<'a, P: plakous_host_protocol::ProtocolExtension> InterruptWatch<'a, P> {
    /// Binds one mailbox to the host-owned observation state.
    pub(crate) const fn new(
        actions: &'a UserActionReceiver<P>,
        state: &'a RefCell<InterruptWatchState<P>>,
    ) -> Self {
        Self {
            actions,
            state,
            defer_urgent: false,
        }
    }

    /// A watch for idle-entry work: every action it drains, the urgent lane
    /// included, is left for the idle dispatcher.
    pub(crate) const fn deferring(
        actions: &'a UserActionReceiver<P>,
        state: &'a RefCell<InterruptWatchState<P>>,
    ) -> Self {
        Self {
            actions,
            state,
            defer_urgent: true,
        }
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
        let _ = self.drain();
    }

    /// [`Self::poll`], answering whether the mailbox can still deliver.
    fn drain(self) -> bool {
        loop {
            match self.actions.try_recv() {
                Ok(delivery) => {
                    let mut state = self.state.borrow_mut();
                    match IntentClass::of(&*delivery).disposition(DispatchPhase::Workflow) {
                        Disposition::ObserveInterrupt if self.defer_urgent => {
                            defer_urgent(&mut state.deferred, delivery, |queued| {
                                IntentClass::of(&**queued) == IntentClass::Urgent
                            });
                        }
                        Disposition::ObserveInterrupt => state.interrupted = true,
                        _ => state.deferred.push_back(delivery),
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
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
                .any(|delivery| IntentClass::of(&**delivery) == IntentClass::Quit)
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

/// Awaits one provider round-trip while observing the control mailbox.
///
/// The round-trip itself is never cancelled: a paid canonical operation must
/// reach its own receipt so its outcome stays lawful. Only the
/// *next* step is skipped, by the caller, once [`InterruptWatch::interrupted`]
/// reports the intent. The mailbox is drained as intents arrive, never on a
/// timer: the urgent lane is recorded and everything else deferred. A
/// mailbox whose producers are gone has nothing left to observe.
pub(crate) async fn watched<F, P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    future: F,
) -> F::Output
where
    F: Future,
{
    tokio::pin!(future);
    let mut observing = true;
    loop {
        tokio::select! {
            biased;
            output = &mut future => return output,
            () = watch.actions.ready(), if observing => {
                observing = watch.drain();
            }
        }
    }
}

/// What the host actually observed about one cancel request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CancelSettlement {
    /// The runtime joined the cancel; `interrupted` may be asserted.
    Settled,
    /// The cancel timed out or was refused; the turn may still be running.
    Unsettled(CancelUnsettled),
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
        Ok(Err(error)) => CancelSettlement::Unsettled(CancelUnsettled::RuntimeRefused {
            cause: ControlFailure {
                diagnostic: DiagnosticText::new(error.to_string()),
            },
        }),
        Err(_) => CancelSettlement::Unsettled(CancelUnsettled::Deadline {
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        }),
    }
}

#[cfg(test)]
mod tests;
