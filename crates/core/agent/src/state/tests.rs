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

    let terminal = machine.transition(TurnTransition::ResolveFailure(FailureResolution::Failed))?;
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
