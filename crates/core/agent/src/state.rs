//! Crate-private lifecycle kernel for one already-admitted turn.
//!
//! This module deliberately owns no driver queue, ledger head, durability,
//! events, I/O, or provider state. The production driver applies these exact
//! transitions around those side effects and consumes the terminal value once.

use std::error::Error;
use std::fmt;

use plakous_model::TurnId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnPhase {
    Accepted,
    Queued,
    Preparing,
    ModelStreaming,
    ToolsRunning,
    Committing,
    Cancelling,
    Failing,
    Completed,
    Cancelled,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancellationResolution {
    /// All owned work joined and no external effect remains indeterminate.
    JoinedCleanly,
    /// A dispatched external effect cannot be classified safely.
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailureResolution {
    /// Cleanup joined and the original failure remains authoritative.
    Failed,
    /// Cleanup found an external effect with an indeterminate outcome.
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnTransition {
    Enqueue,
    BeginPreparing,
    BeginModelStreaming,
    BeginTools,
    /// A complete no-tool response accepted steering and starts another round.
    ContinueAfterModel,
    BeginCommit,
    BeginCancellation,
    BeginFailure,
    CommitSucceeded,
    ResolveCancellation(CancellationResolution),
    ResolveFailure(FailureResolution),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalOutcome {
    Completed,
    Cancelled,
    Failed,
    OutcomeUnknown,
}

impl TerminalOutcome {
    const fn phase(self) -> TurnPhase {
        match self {
            Self::Completed => TurnPhase::Completed,
            Self::Cancelled => TurnPhase::Cancelled,
            Self::Failed => TurnPhase::Failed,
            Self::OutcomeUnknown => TurnPhase::OutcomeUnknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalTurn {
    turn_id: TurnId,
    outcome: TerminalOutcome,
}

impl TerminalTurn {
    const fn new(turn_id: TurnId, outcome: TerminalOutcome) -> Self {
        Self { turn_id, outcome }
    }

    pub(crate) const fn turn_id(self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn outcome(self) -> TerminalOutcome {
        self.outcome
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransitionRecord {
    turn_id: TurnId,
    previous: TurnPhase,
    current: TurnPhase,
    terminal: Option<TerminalOutcome>,
}

impl TransitionRecord {
    const fn new(
        turn_id: TurnId,
        previous: TurnPhase,
        current: TurnPhase,
        terminal: Option<TerminalOutcome>,
    ) -> Self {
        Self {
            turn_id,
            previous,
            current,
            terminal,
        }
    }

    pub(crate) const fn turn_id(self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn previous(self) -> TurnPhase {
        self.previous
    }

    pub(crate) const fn current(self) -> TurnPhase {
        self.current
    }

    /// Returns an outcome only for the unique terminalizing transition.
    pub(crate) const fn terminal(self) -> Option<TerminalOutcome> {
        self.terminal
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LivePhase {
    Accepted,
    Queued,
    Preparing,
    ModelStreaming,
    ToolsRunning,
    Committing,
    Cancelling { origin: TurnPhase },
    Failing { origin: TurnPhase },
}

impl LivePhase {
    const fn public(self) -> TurnPhase {
        match self {
            Self::Accepted => TurnPhase::Accepted,
            Self::Queued => TurnPhase::Queued,
            Self::Preparing => TurnPhase::Preparing,
            Self::ModelStreaming => TurnPhase::ModelStreaming,
            Self::ToolsRunning => TurnPhase::ToolsRunning,
            Self::Committing => TurnPhase::Committing,
            Self::Cancelling { .. } => TurnPhase::Cancelling,
            Self::Failing { .. } => TurnPhase::Failing,
        }
    }

    const fn cleanup_origin(self) -> TurnPhase {
        match self {
            Self::Accepted => TurnPhase::Accepted,
            Self::Queued => TurnPhase::Queued,
            Self::Preparing => TurnPhase::Preparing,
            Self::ModelStreaming => TurnPhase::ModelStreaming,
            Self::ToolsRunning => TurnPhase::ToolsRunning,
            Self::Committing => TurnPhase::Committing,
            Self::Cancelling { origin } | Self::Failing { origin } => origin,
        }
    }

    const fn is_operational(self) -> bool {
        matches!(
            self,
            Self::Accepted
                | Self::Queued
                | Self::Preparing
                | Self::ModelStreaming
                | Self::ToolsRunning
                | Self::Committing
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnState {
    Live(LivePhase),
    Terminal { terminal: TerminalTurn, taken: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnStateErrorKind {
    InvalidTransition,
    AlreadyTerminal,
    TerminalNotReady,
    TerminalAlreadyTaken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnStateError {
    kind: TurnStateErrorKind,
}

impl TurnStateError {
    const fn new(kind: TurnStateErrorKind) -> Self {
        Self { kind }
    }

    pub(crate) const fn kind(self) -> TurnStateErrorKind {
        self.kind
    }
}

impl fmt::Display for TurnStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            TurnStateErrorKind::InvalidTransition => {
                "turn transition is invalid from the current phase"
            }
            TurnStateErrorKind::AlreadyTerminal => "turn already terminalized",
            TurnStateErrorKind::TerminalNotReady => "turn has not terminalized",
            TurnStateErrorKind::TerminalAlreadyTaken => {
                "the unique terminal result was already taken"
            }
        };
        formatter.write_str(message)
    }
}

impl Error for TurnStateError {}

/// Pure lifecycle state for one exact turn whose admission already succeeded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TurnMachine {
    turn_id: TurnId,
    state: TurnState,
}

impl TurnMachine {
    pub(crate) const fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            state: TurnState::Live(LivePhase::Accepted),
        }
    }

    pub(crate) const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn phase(&self) -> TurnPhase {
        match self.state {
            TurnState::Live(phase) => phase.public(),
            TurnState::Terminal { terminal, .. } => terminal.outcome().phase(),
        }
    }

    /// Returns the phase from which cancellation/failure cleanup began.
    pub(crate) const fn cleanup_origin(&self) -> Option<TurnPhase> {
        match self.state {
            TurnState::Live(phase @ (LivePhase::Cancelling { .. } | LivePhase::Failing { .. })) => {
                Some(phase.cleanup_origin())
            }
            TurnState::Live(_) | TurnState::Terminal { .. } => None,
        }
    }

    /// Returns the terminal value until its unique take operation.
    pub(crate) const fn terminal(&self) -> Option<TerminalTurn> {
        match self.state {
            TurnState::Terminal {
                terminal,
                taken: false,
            } => Some(terminal),
            TurnState::Terminal { taken: true, .. } | TurnState::Live(_) => None,
        }
    }

    /// Applies one exact lifecycle transition without performing side effects.
    ///
    /// Rejected transitions leave the machine unchanged. Cancellation and
    /// failure do not terminalize until the caller supplies an explicit cleanup
    /// resolution. In particular, tool-running cancellation has no implicit
    /// clean outcome and can resolve as [`TerminalOutcome::OutcomeUnknown`].
    pub(crate) fn transition(
        &mut self,
        transition: TurnTransition,
    ) -> Result<TransitionRecord, TurnStateError> {
        let TurnState::Live(live) = self.state else {
            return Err(TurnStateError::new(TurnStateErrorKind::AlreadyTerminal));
        };
        let previous = live.public();
        let (next, terminal) = self.next(live, transition)?;
        let current = match next {
            TurnState::Live(phase) => phase.public(),
            TurnState::Terminal { terminal, .. } => terminal.outcome().phase(),
        };
        self.state = next;
        Ok(TransitionRecord::new(
            self.turn_id,
            previous,
            current,
            terminal,
        ))
    }

    /// Validates an ordered transition path without mutating or consuming this
    /// machine. The returned terminal value is descriptive only; the caller
    /// must still apply the path and perform the unique terminal take.
    pub(crate) fn preview_transitions(
        &self,
        transitions: &[TurnTransition],
    ) -> Result<Option<TerminalTurn>, TurnStateError> {
        let mut state = self.state;
        for transition in transitions {
            let TurnState::Live(live) = state else {
                return Err(TurnStateError::new(TurnStateErrorKind::AlreadyTerminal));
            };
            (state, _) = self.next(live, *transition)?;
        }
        Ok(match state {
            TurnState::Terminal {
                terminal,
                taken: false,
            } => Some(terminal),
            TurnState::Live(_) | TurnState::Terminal { taken: true, .. } => None,
        })
    }

    /// Takes the unique terminal value exactly once.
    pub(crate) fn take_terminal(&mut self) -> Result<TerminalTurn, TurnStateError> {
        match self.state {
            TurnState::Live(_) => Err(TurnStateError::new(TurnStateErrorKind::TerminalNotReady)),
            TurnState::Terminal {
                terminal,
                taken: false,
            } => {
                self.state = TurnState::Terminal {
                    terminal,
                    taken: true,
                };
                Ok(terminal)
            }
            TurnState::Terminal { taken: true, .. } => Err(TurnStateError::new(
                TurnStateErrorKind::TerminalAlreadyTaken,
            )),
        }
    }

    fn next(
        &self,
        live: LivePhase,
        transition: TurnTransition,
    ) -> Result<(TurnState, Option<TerminalOutcome>), TurnStateError> {
        let next_live = match (live, transition) {
            (LivePhase::Accepted, TurnTransition::Enqueue) => LivePhase::Queued,
            (LivePhase::Queued | LivePhase::ToolsRunning, TurnTransition::BeginPreparing) => {
                LivePhase::Preparing
            }
            (LivePhase::Preparing, TurnTransition::BeginModelStreaming) => {
                LivePhase::ModelStreaming
            }
            (LivePhase::ModelStreaming, TurnTransition::BeginTools) => LivePhase::ToolsRunning,
            (LivePhase::ModelStreaming, TurnTransition::ContinueAfterModel) => LivePhase::Preparing,
            (LivePhase::ModelStreaming, TurnTransition::BeginCommit) => LivePhase::Committing,
            (phase, TurnTransition::BeginCancellation) if phase.is_operational() => {
                LivePhase::Cancelling {
                    origin: phase.public(),
                }
            }
            (phase, TurnTransition::BeginFailure)
                if phase.is_operational() || matches!(phase, LivePhase::Cancelling { .. }) =>
            {
                LivePhase::Failing {
                    origin: phase.cleanup_origin(),
                }
            }
            (LivePhase::Committing, TurnTransition::CommitSucceeded) => {
                return Ok(self.terminalized(TerminalOutcome::Completed));
            }
            (LivePhase::Cancelling { .. }, TurnTransition::ResolveCancellation(resolution)) => {
                let outcome = match resolution {
                    CancellationResolution::JoinedCleanly => TerminalOutcome::Cancelled,
                    CancellationResolution::OutcomeUnknown => TerminalOutcome::OutcomeUnknown,
                };
                return Ok(self.terminalized(outcome));
            }
            (LivePhase::Failing { .. }, TurnTransition::ResolveFailure(resolution)) => {
                let outcome = match resolution {
                    FailureResolution::Failed => TerminalOutcome::Failed,
                    FailureResolution::OutcomeUnknown => TerminalOutcome::OutcomeUnknown,
                };
                return Ok(self.terminalized(outcome));
            }
            _ => {
                return Err(TurnStateError::new(TurnStateErrorKind::InvalidTransition));
            }
        };
        Ok((TurnState::Live(next_live), None))
    }

    const fn terminalized(&self, outcome: TerminalOutcome) -> (TurnState, Option<TerminalOutcome>) {
        (
            TurnState::Terminal {
                terminal: TerminalTurn::new(self.turn_id, outcome),
                taken: false,
            },
            Some(outcome),
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
