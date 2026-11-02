//! How the host takes operator intents, in one table.
//!
//! Three loops take intents from the control mailbox: the idle loop, the
//! drive loop of a running turn, and a long workflow (compact, fork, rebind,
//! pre-turn preparation) under [`crate::interrupt::run_watched`]. They share
//! one reading order and one classification:
//!
//! * Every pass takes at most one action: a deferred one first, in arrival
//!   order, and only then the mailbox ([`take_next`]).
//! * [`IntentClass::of`] classifies each intent; [`IntentClass::disposition`]
//!   says what a running turn or a workflow does with that class. The idle
//!   loop handles every class itself.
//! * A workflow observes only the urgent `Interrupt` and defers everything
//!   else, `Quit` included, back to the loop that takes the next pass.
//! * `Quit` keeps its place in that FIFO, but preparation never starts a
//!   turn ahead of a deferred `Quit` ([`crate::interrupt::InterruptWatch`]).

use std::cell::RefCell;

use crate::interrupt::InterruptWatchState;
use crate::surface_protocol::{UserAction, UserActionDelivery};

/// What an intent is to the loops that take it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IntentClass {
    /// `Interrupt`: taken at once, by a workflow too.
    Urgent,
    /// `Quit`: taken in arrival order after work already accepted.
    Quit,
    /// Refused while a turn runs; `noun` names it in the refusal note.
    IdleOnly { noun: &'static str },
    /// Handled whenever a loop takes it, idle or with a turn running.
    Ordinary,
}

/// A loop that does not handle every intent itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchPhase {
    /// The drive loop of a running root turn.
    Busy,
    /// A long workflow between its provider round-trips.
    Workflow,
}

/// What a loop does with one intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Disposition {
    /// The loop handles it now.
    Handle,
    /// The loop answers with an "available when idle" note naming `noun`.
    RefuseWhileBusy { noun: &'static str },
    /// The workflow records the interrupt for its next checkpoint.
    ObserveInterrupt,
    /// The workflow leaves it, in arrival order, for the next pass.
    Defer,
}

impl IntentClass {
    /// The class of `action`. Exhaustive: a new intent must be classified.
    pub(crate) fn of<P: plakous_host_protocol::ProtocolExtension>(action: &UserAction<P>) -> Self {
        match action {
            UserAction::Interrupt => Self::Urgent,
            UserAction::Quit => Self::Quit,
            UserAction::ClearSession => Self::IdleOnly { noun: "clear" },
            UserAction::Compact => Self::IdleOnly { noun: "compact" },
            UserAction::ResumeSession { .. } | UserAction::ResumeSessionHandoff { .. } => {
                Self::IdleOnly { noun: "resume" }
            }
            UserAction::ForkSession { .. } => Self::IdleOnly { noun: "fork" },
            UserAction::InvokeTool { .. } => Self::IdleOnly {
                noun: "invoke_tool",
            },
            UserAction::RetryAttach => Self::IdleOnly { noun: "binding" },
            UserAction::Recover => Self::IdleOnly { noun: "recover" },
            UserAction::Prompt(_)
            | UserAction::FollowUp(_)
            | UserAction::Steer(_)
            | UserAction::Background
            | UserAction::StopProcesses { .. }
            | UserAction::DropQueued
            | UserAction::SendQueuedNow { .. }
            | UserAction::DeleteQueued { .. }
            | UserAction::EditQueued { .. }
            | UserAction::MoveQueued { .. }
            | UserAction::ResumeQueueDrain
            | UserAction::ResolveApproval { .. }
            | UserAction::RefreshReadiness
            | UserAction::OpenTopologyNavigator
            | UserAction::OpenAgentTranscript { .. }
            | UserAction::CloseAgentTranscript
            | UserAction::ResyncTranscript { .. }
            | UserAction::RefreshTopologyTree
            | UserAction::ResumeTopologyTree
            | UserAction::InterruptTopologyNode { .. }
            | UserAction::CloseTopologyNode { .. }
            | UserAction::StopAllTopology
            | UserAction::RecoverTopologyNode { .. }
            | UserAction::RefreshTemplates
            | UserAction::RequestCallDetail { .. }
            | UserAction::Extension(_) => Self::Ordinary,
        }
    }

    /// The noun an idle-only intent's refusal names.
    pub(crate) const fn idle_only_noun(self) -> Option<&'static str> {
        match self {
            Self::IdleOnly { noun } => Some(noun),
            Self::Urgent | Self::Quit | Self::Ordinary => None,
        }
    }

    /// What `phase` does with an intent of this class.
    pub(crate) const fn disposition(self, phase: DispatchPhase) -> Disposition {
        match (phase, self) {
            (DispatchPhase::Workflow, Self::Urgent) => Disposition::ObserveInterrupt,
            (DispatchPhase::Workflow, _) => Disposition::Defer,
            (DispatchPhase::Busy, Self::IdleOnly { noun }) => Disposition::RefuseWhileBusy { noun },
            (DispatchPhase::Busy, _) => Disposition::Handle,
        }
    }
}

/// Takes the one action a pass handles: the oldest deferred action, else
/// whatever `mailbox` yields. A deferred action arrived before anything
/// still in the mailbox, so this order holds across a turn boundary too.
pub(crate) fn take_next<P: plakous_host_protocol::ProtocolExtension, E>(
    deferred: &RefCell<InterruptWatchState<P>>,
    mailbox: impl FnOnce() -> Result<UserActionDelivery<P>, E>,
) -> Result<UserActionDelivery<P>, E> {
    let deferred = deferred.borrow_mut().take_deferred();
    deferred.map_or_else(mailbox, Ok)
}

#[cfg(test)]
mod tests;
