//! Recovery reads of recorded round decisions (spec 01 §5.5). Every reader
//! here reads the decision the live driver recorded on the turn's newest
//! round provider; none classifies a response, reads a finish reason or tool
//! call blocks, or reads history to decide what a round asked for.

use super::{
    AgentError, DurableOperationKind, DurableOperationRecord, DurableOperationState, OperationId,
    RuntimeOperationLedger, TurnId, controls, operation_turn_id,
};
use crate::runtime::operation_record::{ProviderOutcome, RoundDecision, provider_outcome};
use plakous_model::ModelOperationRoot;

/// T's newest round provider operation, P. A compaction's own provider
/// attempt is a compaction record, not a round.
pub(super) fn newest_round(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> Option<&DurableOperationRecord> {
    ledger
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(record.kind(), DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(bound), ..
            } if *bound == turn_id)
        })
        .max_by_key(|record| record.operation_id())
}

/// P's recorded outcome in any terminal attempt state.
///
/// # Errors
///
/// A record whose outcome does not decode is typed integrity.
pub(super) fn newest_decision(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> Result<Option<(&DurableOperationRecord, ProviderOutcome)>, AgentError> {
    let Some(record) = newest_round(ledger, turn_id) else {
        return Ok(None);
    };
    if !matches!(
        record.state(),
        DurableOperationState::Completed(_)
            | DurableOperationState::ProvenNotDispatched(_)
            | DurableOperationState::OutcomeUnknown(_)
            | DurableOperationState::Finalized(_)
    ) {
        return Ok(None);
    }
    Ok(provider_outcome(record)?.map(|outcome| (record, outcome)))
}

/// P is `Completed` with a `ToolBatch` decision: the round asked the harness
/// for tools, and their exchange was never promoted.
pub(super) fn completed_tool_batch(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> Result<bool, AgentError> {
    Ok(
        newest_decision(ledger, turn_id)?.is_some_and(|(record, outcome)| {
            matches!(record.state(), DurableOperationState::Completed(_))
                && outcome.decision == RoundDecision::ToolBatch
        }),
    )
}

/// The round boundary a dispatched root rests on when P is `Finalized` with
/// `ToolBatch` or `Continue` and nothing after it is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum RoundBoundary {
    /// A promoted tool exchange: `ToolRoundCompleted`.
    Tool,
    /// A promoted assistant continuation: the model-round wording.
    Model,
}

/// The finalized-round boundary of the closure table: root `R` dispatched, P
/// finalized with `ToolBatch` or `Continue`, and every other same-turn record
/// finalized, an accepted control, or (after a tool batch) a live process
/// session.
///
/// # Errors
///
/// A record whose outcome does not decode is typed integrity.
pub(in crate::runtime) fn finalized_round_boundary(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
    root: OperationId,
) -> Result<Option<RoundBoundary>, AgentError> {
    let records = ledger.operations();
    if !records.iter().any(|record| {
        record.operation_id() == root.0
            && matches!(record.state(), DurableOperationState::Dispatched)
    }) {
        return Ok(None);
    }
    let Some((provider, outcome)) = newest_decision(ledger, turn_id)? else {
        return Ok(None);
    };
    if !matches!(provider.state(), DurableOperationState::Finalized(_)) {
        return Ok(None);
    }
    let boundary = match outcome.decision {
        RoundDecision::ToolBatch => RoundBoundary::Tool,
        RoundDecision::Continue => RoundBoundary::Model,
        _ => return Ok(None),
    };
    let closed = records.iter().all(|record| {
        record.operation_id() == root.0
            || matches!(record.state(), DurableOperationState::Finalized(_))
            || controls::same_turn_control(ledger, record, turn_id)
            || (boundary == RoundBoundary::Tool
                && operation_turn_id(record) == Some(turn_id)
                && matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
                && matches!(
                    record.state(),
                    DurableOperationState::Prepared | DurableOperationState::Dispatched
                ))
    });
    Ok(closed.then_some(boundary))
}

/// A retained earlier round, for a note that says so: a same-turn provider
/// `Finalized` with `ToolBatch` or `Continue`.
///
/// # Errors
///
/// A record whose outcome does not decode is typed integrity.
pub(super) fn retained_round(record: &DurableOperationRecord) -> Result<bool, AgentError> {
    if !matches!(record.state(), DurableOperationState::Finalized(_)) {
        return Ok(false);
    }
    Ok(provider_outcome(record)?.is_some_and(|outcome| {
        matches!(
            outcome.decision,
            RoundDecision::ToolBatch | RoundDecision::Continue
        )
    }))
}

/// A decision whose consequence was a later dispatch that never happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum UnsentDecision {
    /// `RetryOutputLimit` or `RetryContentFilter`: the decided retry was not
    /// sent.
    Retry,
    /// `OverflowFold` with no compaction record: compaction did not start.
    Fold,
}

/// The decided-but-unsent rows of the closure table: root R dispatched, P
/// finalized with a retry decision or settled with an overflow-fold
/// decision, and nothing after P except same-turn accepted controls.
///
/// # Errors
///
/// A record whose outcome does not decode is typed integrity.
pub(in crate::runtime) fn unsent_decision(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
    root: OperationId,
) -> Result<Option<UnsentDecision>, AgentError> {
    let records = ledger.operations();
    if !records.iter().any(|record| {
        record.operation_id() == root.0
            && matches!(record.state(), DurableOperationState::Dispatched)
    }) {
        return Ok(None);
    }
    let Some((provider, outcome)) = newest_decision(ledger, turn_id)? else {
        return Ok(None);
    };
    let finalized = matches!(provider.state(), DurableOperationState::Finalized(_));
    let unsent = match outcome.decision {
        RoundDecision::RetryOutputLimit | RoundDecision::RetryContentFilter if finalized => {
            UnsentDecision::Retry
        }
        // The fold decision leaves P Completed until the fold or the turn's
        // terminal commit finalizes it.
        RoundDecision::OverflowFold => UnsentDecision::Fold,
        _ => return Ok(None),
    };
    let closed = records.iter().all(|record| {
        record.operation_id() == root.0
            || controls::same_turn_control(ledger, record, turn_id)
            || (record.operation_id() < provider.operation_id()
                && matches!(record.state(), DurableOperationState::Finalized(_)))
            || record.operation_id() == provider.operation_id()
    });
    Ok(closed.then_some(unsent))
}
