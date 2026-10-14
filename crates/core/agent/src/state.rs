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
mod tests {
    use super::*;
    use proptest::prelude::*;
    use uuid::Uuid;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    const ALL_TRANSITIONS: [TurnTransition; 13] = [
        TurnTransition::Enqueue,
        TurnTransition::BeginPreparing,
        TurnTransition::BeginModelStreaming,
        TurnTransition::BeginTools,
        TurnTransition::ContinueAfterModel,
        TurnTransition::BeginCommit,
        TurnTransition::BeginCancellation,
        TurnTransition::BeginFailure,
        TurnTransition::CommitSucceeded,
        TurnTransition::ResolveCancellation(CancellationResolution::JoinedCleanly),
        TurnTransition::ResolveCancellation(CancellationResolution::OutcomeUnknown),
        TurnTransition::ResolveFailure(FailureResolution::Failed),
        TurnTransition::ResolveFailure(FailureResolution::OutcomeUnknown),
    ];

    fn generated_transition(code: u8) -> TurnTransition {
        match code % 13 {
            0 => TurnTransition::Enqueue,
            1 => TurnTransition::BeginPreparing,
            2 => TurnTransition::BeginModelStreaming,
            3 => TurnTransition::BeginTools,
            4 => TurnTransition::ContinueAfterModel,
            5 => TurnTransition::BeginCommit,
            6 => TurnTransition::BeginCancellation,
            7 => TurnTransition::BeginFailure,
            8 => TurnTransition::CommitSucceeded,
            9 => TurnTransition::ResolveCancellation(CancellationResolution::JoinedCleanly),
            10 => TurnTransition::ResolveCancellation(CancellationResolution::OutcomeUnknown),
            11 => TurnTransition::ResolveFailure(FailureResolution::Failed),
            _ => TurnTransition::ResolveFailure(FailureResolution::OutcomeUnknown),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn generated_transition_schedules_are_rejection_atomic_and_terminal_take_once(
            schedule in prop::collection::vec(any::<u8>(), 0..96),
        ) {
            let id = turn_id(77)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            let mut machine = TurnMachine::new(id);
            let mut terminal_count = 0_u8;

            for code in schedule {
                let before = machine.clone();
                let previous = machine.phase();
                if let Ok(record) = machine.transition(generated_transition(code)) {
                    prop_assert_eq!(record.turn_id(), id);
                    prop_assert_eq!(record.previous(), previous);
                    prop_assert_eq!(record.current(), machine.phase());
                    if let Some(outcome) = record.terminal() {
                        terminal_count = terminal_count.saturating_add(1);
                        prop_assert_eq!(terminal_count, 1);
                        prop_assert_eq!(
                            machine.terminal().map(TerminalTurn::outcome),
                            Some(outcome),
                        );
                        let taken = machine
                            .take_terminal()
                            .map_err(|error| TestCaseError::fail(error.to_string()))?;
                        prop_assert_eq!(taken.turn_id(), id);
                        prop_assert_eq!(taken.outcome(), outcome);
                        prop_assert!(machine.terminal().is_none());
                        prop_assert_eq!(
                            machine.take_terminal().map_err(TurnStateError::kind),
                            Err(TurnStateErrorKind::TerminalAlreadyTaken),
                        );
                    }
                } else {
                    prop_assert_eq!(&machine, &before);
                }
            }

            prop_assert!(terminal_count <= 1);
        }
    }

    const LIVE_PHASES: [TurnPhase; 8] = [
        TurnPhase::Accepted,
        TurnPhase::Queued,
        TurnPhase::Preparing,
        TurnPhase::ModelStreaming,
        TurnPhase::ToolsRunning,
        TurnPhase::Committing,
        TurnPhase::Cancelling,
        TurnPhase::Failing,
    ];

    fn turn_id(value: u128) -> Result<TurnId, plakous_model::IdentityError> {
        TurnId::try_from(Uuid::from_u128(value))
    }

    fn machine_at(phase: TurnPhase) -> TestResult<TurnMachine> {
        let mut machine = TurnMachine::new(turn_id(1)?);
        let transitions: &[TurnTransition] = match phase {
            TurnPhase::Accepted => &[],
            TurnPhase::Queued => &[TurnTransition::Enqueue],
            TurnPhase::Preparing => &[TurnTransition::Enqueue, TurnTransition::BeginPreparing],
            TurnPhase::ModelStreaming => &[
                TurnTransition::Enqueue,
                TurnTransition::BeginPreparing,
                TurnTransition::BeginModelStreaming,
            ],
            TurnPhase::ToolsRunning => &[
                TurnTransition::Enqueue,
                TurnTransition::BeginPreparing,
                TurnTransition::BeginModelStreaming,
                TurnTransition::BeginTools,
            ],
            TurnPhase::Committing => &[
                TurnTransition::Enqueue,
                TurnTransition::BeginPreparing,
                TurnTransition::BeginModelStreaming,
                TurnTransition::BeginCommit,
            ],
            TurnPhase::Cancelling => &[TurnTransition::BeginCancellation],
            TurnPhase::Failing => &[TurnTransition::BeginFailure],
            TurnPhase::Completed
            | TurnPhase::Cancelled
            | TurnPhase::Failed
            | TurnPhase::OutcomeUnknown => return Err("terminal phase requested as live".into()),
        };
        for transition in transitions {
            machine.transition(*transition)?;
        }
        Ok(machine)
    }

    fn transition_is_legal(phase: TurnPhase, transition: TurnTransition) -> bool {
        match phase {
            TurnPhase::Accepted => matches!(
                transition,
                TurnTransition::Enqueue
                    | TurnTransition::BeginCancellation
                    | TurnTransition::BeginFailure
            ),
            TurnPhase::Queued => matches!(
                transition,
                TurnTransition::BeginPreparing
                    | TurnTransition::BeginCancellation
                    | TurnTransition::BeginFailure
            ),
            TurnPhase::Preparing => matches!(
                transition,
                TurnTransition::BeginModelStreaming
                    | TurnTransition::BeginCancellation
                    | TurnTransition::BeginFailure
            ),
            TurnPhase::ModelStreaming => matches!(
                transition,
                TurnTransition::BeginTools
                    | TurnTransition::ContinueAfterModel
                    | TurnTransition::BeginCommit
                    | TurnTransition::BeginCancellation
                    | TurnTransition::BeginFailure
            ),
            TurnPhase::ToolsRunning => matches!(
                transition,
                TurnTransition::BeginPreparing
                    | TurnTransition::BeginCancellation
                    | TurnTransition::BeginFailure
            ),
            TurnPhase::Committing => matches!(
                transition,
                TurnTransition::BeginCancellation
                    | TurnTransition::BeginFailure
                    | TurnTransition::CommitSucceeded
            ),
            TurnPhase::Cancelling => matches!(
                transition,
                TurnTransition::BeginFailure | TurnTransition::ResolveCancellation(_)
            ),
            TurnPhase::Failing => matches!(transition, TurnTransition::ResolveFailure(_)),
            TurnPhase::Completed
            | TurnPhase::Cancelled
            | TurnPhase::Failed
            | TurnPhase::OutcomeUnknown => false,
        }
    }

    #[test]
    fn transition_matrix_exhaustively_accepts_only_legal_edges() -> TestResult {
        for phase in LIVE_PHASES {
            for transition in ALL_TRANSITIONS {
                let mut machine = machine_at(phase)?;
                let before = (
                    machine.phase(),
                    machine.cleanup_origin(),
                    machine.terminal(),
                );
                let result = machine.transition(transition);
                if transition_is_legal(phase, transition) {
                    let record = result?;
                    assert_eq!(record.turn_id(), machine.turn_id());
                    assert_eq!(record.previous(), phase);
                    assert_eq!(record.current(), machine.phase());
                } else {
                    assert_eq!(
                        result.map_err(TurnStateError::kind),
                        Err(TurnStateErrorKind::InvalidTransition),
                        "unexpected result for {phase:?} + {transition:?}"
                    );
                    assert_eq!(
                        (
                            machine.phase(),
                            machine.cleanup_origin(),
                            machine.terminal(),
                        ),
                        before
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn full_tool_loop_returns_to_model_then_commits() -> TestResult {
        let turn_id = turn_id(2)?;
        let mut machine = TurnMachine::new(turn_id);
        let expected = [
            (TurnTransition::Enqueue, TurnPhase::Queued),
            (TurnTransition::BeginPreparing, TurnPhase::Preparing),
            (
                TurnTransition::BeginModelStreaming,
                TurnPhase::ModelStreaming,
            ),
            (TurnTransition::BeginTools, TurnPhase::ToolsRunning),
            (TurnTransition::BeginPreparing, TurnPhase::Preparing),
            (
                TurnTransition::BeginModelStreaming,
                TurnPhase::ModelStreaming,
            ),
            (TurnTransition::BeginCommit, TurnPhase::Committing),
            (TurnTransition::CommitSucceeded, TurnPhase::Completed),
        ];

        for (transition, phase) in expected {
            let record = machine.transition(transition)?;
            assert_eq!(record.current(), phase);
            assert_eq!(
                record.terminal(),
                (phase == TurnPhase::Completed).then_some(TerminalOutcome::Completed)
            );
        }
        let terminal = machine.take_terminal()?;
        assert_eq!(terminal.turn_id(), turn_id);
        assert_eq!(terminal.outcome(), TerminalOutcome::Completed);
        Ok(())
    }

    #[test]
    fn terminal_preview_is_non_consuming_and_authoritative_take_remains_unique() -> TestResult {
        let mut machine = machine_at(TurnPhase::Committing)?;
        let turn_id = machine.turn_id();
        let before = machine.clone();

        let preview = machine
            .preview_transitions(&[TurnTransition::CommitSucceeded])?
            .ok_or("commit preview did not reach a terminal state")?;
        assert_eq!(preview.turn_id(), turn_id);
        assert_eq!(preview.outcome(), TerminalOutcome::Completed);
        assert_eq!(machine, before);
        assert!(machine.terminal().is_none());

        machine.transition(TurnTransition::CommitSucceeded)?;
        assert_eq!(machine.take_terminal()?, preview);
        assert_eq!(
            machine.take_terminal().map_err(TurnStateError::kind),
            Err(TurnStateErrorKind::TerminalAlreadyTaken)
        );
        Ok(())
    }

    #[test]
    fn cancellation_and_failure_are_available_from_every_operational_phase() -> TestResult {
        let operational = [
            TurnPhase::Accepted,
            TurnPhase::Queued,
            TurnPhase::Preparing,
            TurnPhase::ModelStreaming,
            TurnPhase::ToolsRunning,
            TurnPhase::Committing,
        ];
        for phase in operational {
            let mut cancelling = machine_at(phase)?;
            cancelling.transition(TurnTransition::BeginCancellation)?;
            assert_eq!(cancelling.cleanup_origin(), Some(phase));
            let resolution = if phase == TurnPhase::ToolsRunning {
                CancellationResolution::OutcomeUnknown
            } else {
                CancellationResolution::JoinedCleanly
            };
            cancelling.transition(TurnTransition::ResolveCancellation(resolution))?;
            assert_eq!(
                cancelling.phase(),
                if phase == TurnPhase::ToolsRunning {
                    TurnPhase::OutcomeUnknown
                } else {
                    TurnPhase::Cancelled
                }
            );

            let mut failing = machine_at(phase)?;
            failing.transition(TurnTransition::BeginFailure)?;
            assert_eq!(failing.cleanup_origin(), Some(phase));
            failing.transition(TurnTransition::ResolveFailure(FailureResolution::Failed))?;
            assert_eq!(failing.phase(), TurnPhase::Failed);
        }
        Ok(())
    }

    #[test]
    fn cancellation_cleanup_can_escalate_to_failure_without_losing_origin() -> TestResult {
        let mut machine = machine_at(TurnPhase::ToolsRunning)?;
        machine.transition(TurnTransition::BeginCancellation)?;
        machine.transition(TurnTransition::BeginFailure)?;
        assert_eq!(machine.cleanup_origin(), Some(TurnPhase::ToolsRunning));
        let record = machine.transition(TurnTransition::ResolveFailure(
            FailureResolution::OutcomeUnknown,
        ))?;
        assert_eq!(record.terminal(), Some(TerminalOutcome::OutcomeUnknown));
        Ok(())
    }

    #[test]
    fn cancellation_cleanup_failure_transitions_through_failing() -> TestResult {
        let mut machine = machine_at(TurnPhase::ModelStreaming)?;
        machine.transition(TurnTransition::BeginCancellation)?;
        let failing = machine.transition(TurnTransition::BeginFailure)?;

        assert_eq!(failing.previous(), TurnPhase::Cancelling);
        assert_eq!(failing.current(), TurnPhase::Failing);
        assert_eq!(machine.cleanup_origin(), Some(TurnPhase::ModelStreaming));

        let terminal =
            machine.transition(TurnTransition::ResolveFailure(FailureResolution::Failed))?;
        assert_eq!(terminal.terminal(), Some(TerminalOutcome::Failed));
        Ok(())
    }

    #[test]
    fn tool_cancellation_requires_an_explicit_fail_closed_resolution() -> TestResult {
        let mut machine = machine_at(TurnPhase::ToolsRunning)?;
        let cancelling = machine.transition(TurnTransition::BeginCancellation)?;
        assert_eq!(cancelling.terminal(), None);
        assert_eq!(machine.phase(), TurnPhase::Cancelling);
        assert_eq!(machine.cleanup_origin(), Some(TurnPhase::ToolsRunning));
        assert_eq!(machine.terminal(), None);
        assert_eq!(
            machine.take_terminal().map_err(TurnStateError::kind),
            Err(TurnStateErrorKind::TerminalNotReady)
        );

        let terminal = machine.transition(TurnTransition::ResolveCancellation(
            CancellationResolution::OutcomeUnknown,
        ))?;
        assert_eq!(terminal.terminal(), Some(TerminalOutcome::OutcomeUnknown));
        assert_eq!(machine.phase(), TurnPhase::OutcomeUnknown);
        Ok(())
    }

    #[test]
    fn every_terminal_rejects_all_transitions_and_can_be_taken_once() -> TestResult {
        let scenarios = [
            (
                TurnPhase::Committing,
                TurnTransition::CommitSucceeded,
                TerminalOutcome::Completed,
            ),
            (
                TurnPhase::Cancelling,
                TurnTransition::ResolveCancellation(CancellationResolution::JoinedCleanly),
                TerminalOutcome::Cancelled,
            ),
            (
                TurnPhase::Failing,
                TurnTransition::ResolveFailure(FailureResolution::Failed),
                TerminalOutcome::Failed,
            ),
            (
                TurnPhase::Cancelling,
                TurnTransition::ResolveCancellation(CancellationResolution::OutcomeUnknown),
                TerminalOutcome::OutcomeUnknown,
            ),
        ];

        for (start, terminal_transition, outcome) in scenarios {
            let mut machine = machine_at(start)?;
            let record = machine.transition(terminal_transition)?;
            assert_eq!(record.terminal(), Some(outcome));
            assert_eq!(machine.terminal().map(TerminalTurn::outcome), Some(outcome));

            for transition in ALL_TRANSITIONS {
                assert_eq!(
                    machine.transition(transition).map_err(TurnStateError::kind),
                    Err(TurnStateErrorKind::AlreadyTerminal)
                );
            }

            assert_eq!(machine.take_terminal()?.outcome(), outcome);
            assert_eq!(machine.terminal(), None);
            assert_eq!(
                machine.take_terminal().map_err(TurnStateError::kind),
                Err(TurnStateErrorKind::TerminalAlreadyTaken)
            );
            for transition in ALL_TRANSITIONS {
                assert_eq!(
                    machine.transition(transition).map_err(TurnStateError::kind),
                    Err(TurnStateErrorKind::AlreadyTerminal)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn taking_terminal_before_resolution_is_rejected_without_mutation() -> TestResult {
        let mut machine = machine_at(TurnPhase::ToolsRunning)?;
        let before = (
            machine.phase(),
            machine.cleanup_origin(),
            machine.terminal(),
        );
        assert_eq!(
            machine.take_terminal().map_err(TurnStateError::kind),
            Err(TurnStateErrorKind::TerminalNotReady)
        );
        assert_eq!(
            (
                machine.phase(),
                machine.cleanup_origin(),
                machine.terminal(),
            ),
            before
        );
        Ok(())
    }
}
