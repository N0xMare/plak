//! The fold rows of the closure table (spec 01 §5.5): a dispatched root whose
//! turn rests on an admission fold, an overflow fold or a scheduled in-turn
//! fold. Each row is read from the recorded decision and the fold's own
//! records; none reads history to decide what a round asked for.

use super::replay::{self, RoundBoundary};
use super::{
    AbandonedTurnClaim, AbandonmentCause, AgentError, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, ModelOperationRoot, OperationId, RoundDecision, RuntimeCheckpoint,
    TurnId, controls,
};

/// One fold row of the closure table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FoldRow {
    /// R Dispatched, no round provider, open compaction or summary records of
    /// the admission fence: cancelled, `CompactionNotApplied`; the prompt is
    /// not dispatched.
    OpenAdmission,
    /// R Dispatched, no round provider, a `TurnFence` fold committed at
    /// admission: cancelled, `NothingStarted`; the retained fold is named.
    CommittedAdmission,
    /// R Dispatched, P Finalized `OverflowFold`, the fold committed and no
    /// later provider: cancelled, `NothingStarted`; the note names the
    /// committed fold and says the refused request was not sent again.
    CommittedOverflow,
    /// R Dispatched, P Finalized `ToolBatch` or `Continue`, open compaction or
    /// summary records of the turn's scheduled fence: cancelled,
    /// `CompactionNotApplied`; the earlier rounds are named as retained.
    OpenScheduled,
    /// R Dispatched, P Finalized `ToolBatch` or `Continue`, the scheduled fold
    /// committed and no later provider: cancelled, `ToolRoundCompleted`
    /// (model-round wording after `Continue`); the committed fold is named.
    CommittedScheduled(RoundBoundary),
}

/// The state of the fold records prepared after `anchor`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FoldState {
    Open,
    Committed,
}

/// The fold row this cut matches, if any.
///
/// # Errors
///
/// A round provider record whose outcome does not decode is typed integrity.
pub(super) fn fold_row(
    checkpoint: &RuntimeCheckpoint,
    turn_id: TurnId,
    root: OperationId,
) -> Result<Option<FoldRow>, AgentError> {
    let ledger = checkpoint.manifest().ledger();
    let records = ledger.operations();
    if !records.iter().any(|record| {
        record.operation_id() == root.0
            && matches!(record.state(), DurableOperationState::Dispatched)
    }) {
        return Ok(None);
    }
    let Some(round) = replay::newest_round(ledger, turn_id) else {
        return Ok(match fold_state(&records, root.0) {
            Some(FoldState::Open) => Some(FoldRow::OpenAdmission),
            Some(FoldState::Committed) => Some(FoldRow::CommittedAdmission),
            None => None,
        });
    };
    let Some((provider, outcome)) = replay::newest_decision(ledger, turn_id)? else {
        return Ok(None);
    };
    if provider.operation_id() != round.operation_id()
        || !matches!(provider.state(), DurableOperationState::Finalized(_))
    {
        return Ok(None);
    }
    let Some(state) = fold_state(&records, provider.operation_id()) else {
        return Ok(None);
    };
    Ok(match (outcome.decision, state) {
        (RoundDecision::OverflowFold, FoldState::Committed)
            if records.iter().all(|record| {
                record.operation_id() == root.0
                    || matches!(record.state(), DurableOperationState::Finalized(_))
                    || controls::same_turn_control(ledger, record, turn_id)
            }) =>
        {
            Some(FoldRow::CommittedOverflow)
        }
        (RoundDecision::ToolBatch | RoundDecision::Continue, FoldState::Open) => {
            Some(FoldRow::OpenScheduled)
        }
        (RoundDecision::ToolBatch | RoundDecision::Continue, FoldState::Committed) => {
            replay::finalized_round_boundary(ledger, turn_id, root)?
                .map(FoldRow::CommittedScheduled)
        }
        _ => None,
    })
}

/// Whether the compaction records prepared after `anchor` are open, or one
/// fold committed: its own finalization carries successful publication evidence.
fn fold_state(records: &[&DurableOperationRecord], anchor: u64) -> Option<FoldState> {
    let workflow_of = |record: &DurableOperationRecord| match record.kind() {
        DurableOperationKind::Compaction { workflow_id }
        | DurableOperationKind::Provider {
            root: ModelOperationRoot::SessionWorkflow(workflow_id),
            ..
        } => Some(*workflow_id),
        _ => None,
    };
    let fold = records
        .iter()
        .filter(|record| record.operation_id() > anchor && workflow_of(record).is_some())
        .collect::<Vec<_>>();
    if fold.is_empty() {
        return None;
    }
    if fold
        .iter()
        .any(|record| !matches!(record.state(), DurableOperationState::Finalized(_)))
    {
        return Some(FoldState::Open);
    }
    let committed = fold.iter().any(|compaction| {
        let DurableOperationKind::Compaction { workflow_id } = compaction.kind() else {
            return false;
        };
        let DurableOperationState::Finalized(evidence) = compaction.state() else {
            return false;
        };
        serde_json::from_slice::<crate::runtime::compaction::DurableCompactionPublicationEvidence>(
            evidence.bytes(),
        )
        .is_ok_and(|publication| {
            publication.version == 1
                && publication.workflow_id == *workflow_id
                && fold.iter().any(|provider| {
                    matches!(provider.kind(), DurableOperationKind::Provider {
                        root: ModelOperationRoot::SessionWorkflow(bound), ..
                    } if bound == workflow_id)
                })
        }) || crate::runtime::compaction::DurablePlaceholderEvidence::decode_publication(
            evidence.bytes(),
            *workflow_id,
        )
        .is_some()
            || crate::runtime::compaction::residency::record::is_publication(
                compaction,
                *workflow_id,
            )
    });
    committed.then_some(FoldState::Committed)
}

impl FoldRow {
    /// The claim this row's closure makes when nothing stronger is open.
    pub(super) const fn claim(self) -> AbandonedTurnClaim {
        match self {
            Self::OpenAdmission | Self::OpenScheduled => AbandonedTurnClaim::CompactionNotApplied,
            Self::CommittedAdmission | Self::CommittedOverflow => {
                AbandonedTurnClaim::NothingStarted
            }
            Self::CommittedScheduled(_) => AbandonedTurnClaim::ToolRoundCompleted,
        }
    }

    /// The note this row's closure appends, when the row words its own.
    pub(super) const fn note(self, cause: AbandonmentCause) -> Option<&'static str> {
        match self {
            Self::OpenAdmission => None,
            Self::OpenScheduled => Some(match cause {
                AbandonmentCause::ProcessLost => {
                    "this turn was abandoned after the process was lost while the conversation \
                     was being compacted. Compaction was not applied and may have been billed. \
                     Earlier completed rounds of this turn remain in this history; the history \
                     before this point is unchanged."
                }
                AbandonmentCause::Interrupted => {
                    "this turn stopped while the conversation was being compacted. Compaction \
                     was not applied and may have been billed. Earlier completed rounds of this \
                     turn remain in this history; the history before this point is unchanged."
                }
            }),
            Self::CommittedAdmission => Some(
                "this turn was abandoned before its first model request. The committed \
                 compaction remains in history and is not retried; no turn response was \
                 inferred, and the prompt is not retried.",
            ),
            Self::CommittedOverflow => Some(
                "this turn was abandoned after the provider refused its request as over the \
                 context window and the conversation was compacted. The committed compaction \
                 remains in history and is not retried, and the refused request was not sent \
                 again; no turn response was inferred.",
            ),
            Self::CommittedScheduled(RoundBoundary::Tool) => Some(
                "this turn was abandoned after its tool round completed and the conversation \
                 was compacted, before the next model request was prepared. The completed tool \
                 results and the committed compaction remain in this history; they are not \
                 retried.",
            ),
            Self::CommittedScheduled(RoundBoundary::Model) => Some(
                "this turn was abandoned after its model round completed and the conversation \
                 was compacted, before the next model request was prepared. The completed model \
                 response and the committed compaction remain in this history; no next request \
                 was dispatched or retried.",
            ),
        }
    }

    /// The cancellation this row's terminal records, when the row words its
    /// own.
    pub(super) const fn cancellation(self) -> Option<&'static str> {
        match self {
            Self::OpenAdmission | Self::OpenScheduled => None,
            Self::CommittedAdmission => Some(
                "the turn was abandoned before its first model request; committed compaction \
                 was retained",
            ),
            Self::CommittedOverflow => Some(
                "the turn was abandoned after its request was refused as over the context \
                 window; the committed compaction was retained and the refused request was not \
                 sent again",
            ),
            Self::CommittedScheduled(RoundBoundary::Tool) => Some(
                "the turn was interrupted after its tool round completed and the conversation \
                 was compacted, before the next model request",
            ),
            Self::CommittedScheduled(RoundBoundary::Model) => Some(
                "the turn was interrupted after its model round completed and the conversation \
                 was compacted, before the next model request",
            ),
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
