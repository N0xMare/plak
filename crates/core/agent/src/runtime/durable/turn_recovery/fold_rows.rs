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
pub(super) mod tests {
    use super::super::*;
    use super::FoldRow;
    use super::replay::RoundBoundary;
    use crate::runtime::durable::tests::SessionUpdateFixtureMode;
    use crate::runtime::durable::tests::landed_round_abandonment::{
        completed_text_round_cut, completed_tool_round_cut, root_without_provider_cut,
    };
    use crate::runtime::operation_record::test_support::{
        outcome_component, recorded_decision_bytes,
    };
    use plakous_model::{AttemptId, CompactionBlock, SessionWorkflowRootId};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn turn_and_root(cut: &RuntimeCheckpoint) -> TestResult<(TurnId, OperationId)> {
        let RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } = cut.manifest().lifecycle()
        else {
            return Err("working turn".into());
        };
        let root = cut
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| {
                matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) }
                    if bound == turn_id)
            })
            .ok_or("root")?;
        Ok((*turn_id, OperationId(root.operation_id())))
    }

    /// The text round's provider, finalized with `decision` on its recorded
    /// response (or on a failure, for a fold decision).
    pub(in crate::runtime::durable::turn_recovery) fn decided_round(
        source: &RuntimeCheckpoint,
        decision: &RoundDecision,
    ) -> TestResult<RuntimeCheckpoint> {
        let records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|record| {
                if !matches!(record.kind(), DurableOperationKind::Provider { .. }) {
                    return Ok(record.clone());
                }
                let mut transitions = record.transitions().to_vec();
                let Some(DurableOperationState::Completed(evidence)) =
                    transitions.pop().map(|last| last.state().clone())
                else {
                    return Err("completed provider".into());
                };
                let evidence = if matches!(
                    *decision,
                    RoundDecision::OverflowFold
                        | RoundDecision::RetryOutputLimit
                        | RoundDecision::RetryContentFilter
                ) {
                    outcome_component(Err(&AgentError::new(
                        AgentErrorKind::Model,
                        "the request is over the context window",
                    )))?
                } else {
                    evidence
                };
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    recorded_decision_bytes(&evidence, decision.clone())?,
                )?;
                transitions.push(DurableOperationTransition::new(
                    source.id(),
                    DurableOperationState::Completed(evidence.clone()),
                ));
                transitions.push(DurableOperationTransition::new(
                    source.id(),
                    DurableOperationState::Finalized(evidence),
                ));
                Ok(DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    transitions,
                )?)
            })
            .collect::<TestResult<Vec<_>>>()?;
        review_tests::checkpoint_with_review_state(
            source,
            records,
            source.manifest().canonical().history().clone(),
            false,
        )
    }

    /// `source` plus one fold's compaction and summary records, open
    /// (dispatched) or committed (its own publication evidence).
    pub(in crate::runtime::durable::turn_recovery) fn with_fold(
        source: &RuntimeCheckpoint,
        committed: bool,
    ) -> TestResult<RuntimeCheckpoint> {
        let mut records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let workflow_id = SessionWorkflowRootId::new([0xa4; 32])?;
        let digest = source.manifest().canonical().history().identity();
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "workflow_id": workflow_id,
                "source_checkpoint": source.id(),
                "cut": digest,
                "operation": digest,
                "successor_history": digest,
                "after_measurement": digest,
                "after_status": digest,
                "after_input_tokens": 1,
            }))?,
        )?;
        let next = records
            .iter()
            .map(DurableOperationRecord::operation_id)
            .max()
            .ok_or("records")?
            + 1;
        for (operation, kind) in [
            (next, DurableOperationKind::Compaction { workflow_id }),
            (
                next + 1,
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(workflow_id),
                    attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xA4_02))?,
                },
            ),
        ] {
            let mut states = vec![
                DurableOperationState::Prepared,
                DurableOperationState::Dispatched,
            ];
            if committed {
                states.push(DurableOperationState::Completed(evidence.clone()));
                states.push(DurableOperationState::Finalized(evidence.clone()));
            }
            records.push(DurableOperationRecord::new(
                operation,
                kind,
                evidence.clone(),
                states
                    .into_iter()
                    .enumerate()
                    .map(|(index, state)| {
                        Ok(DurableOperationTransition::new(
                            CheckpointId::try_from(uuid::Uuid::from_u128(0xA4_10 + index as u128))?,
                            state,
                        ))
                    })
                    .collect::<TestResult<Vec<_>>>()?,
            )?);
        }
        let mut history = source.manifest().canonical().history().clone();
        if committed {
            history = history.append(HistorySegment::new(
                HistorySegmentKind::Compaction,
                vec![ConversationItem::new(
                    EntryId::try_from(uuid::Uuid::from_u128(0xA4_20))?,
                    Actor::Assistant,
                    vec![ContentBlock::Compaction(CompactionBlock::new(
                        "committed fold summary",
                    )?)],
                    Provenance::ModelOutput,
                )?],
            )?)?;
        }
        review_tests::checkpoint_with_review_state(source, records, history, false)
    }

    #[tokio::test]
    async fn fold_commit_authority_is_its_own_finalization_not_history() -> TestResult {
        let source = root_without_provider_cut(false).await?;
        let committed = with_fold(&source, true)?;
        let earlier_compaction = review_tests::checkpoint_with_review_state(
            &source,
            source
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .cloned()
                .collect(),
            committed.manifest().canonical().history().clone(),
            false,
        )?;
        let uncommitted = with_fold(&earlier_compaction, false)?;
        let (turn, root) = turn_and_root(&uncommitted)?;
        assert_eq!(
            super::fold_state(&uncommitted.manifest().ledger().operations(), root.0),
            Some(super::FoldState::Open)
        );
        assert_eq!(
            super::fold_row(&uncommitted, turn, root)?,
            Some(FoldRow::OpenAdmission)
        );
        let records = committed.manifest().ledger().operations();
        let (_, root) = turn_and_root(&committed)?;
        assert_eq!(
            super::fold_state(&records, root.0),
            Some(super::FoldState::Committed)
        );
        let without_summary = review_tests::checkpoint_with_review_state(
            &committed,
            records.into_iter().cloned().collect(),
            source.manifest().canonical().history().clone(),
            false,
        )?;
        let records = without_summary.manifest().ledger().operations();
        assert_eq!(
            super::fold_state(&records, root.0),
            Some(super::FoldState::Committed)
        );
        let records = committed
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|record| {
                if !matches!(record.kind(), DurableOperationKind::Compaction { .. }) {
                    return Ok(record.clone());
                }
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    b"closed without a publication".to_vec(),
                )?;
                let transitions = record
                    .transitions()
                    .iter()
                    .map(|transition| {
                        DurableOperationTransition::new(
                            transition.checkpoint_id(),
                            match transition.state() {
                                DurableOperationState::Completed(_) => {
                                    DurableOperationState::Completed(evidence.clone())
                                }
                                DurableOperationState::Finalized(_) => {
                                    DurableOperationState::Finalized(evidence.clone())
                                }
                                state => state.clone(),
                            },
                        )
                    })
                    .collect();
                Ok(DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    transitions,
                )?)
            })
            .collect::<TestResult<Vec<_>>>()?;
        let unrelated_summary = review_tests::checkpoint_with_review_state(
            &committed,
            records,
            committed.manifest().canonical().history().clone(),
            false,
        )?;
        assert_eq!(
            super::fold_state(&unrelated_summary.manifest().ledger().operations(), root.0),
            None
        );
        Ok(())
    }

    /// The row the cut matches, the claim and note its closure publishes, and
    /// that the closure dispatches nothing and keeps a committed fold.
    fn assert_row(cut: &RuntimeCheckpoint, row: FoldRow, note: &str) -> TestResult {
        let (turn_id, root) = turn_and_root(cut)?;
        assert_eq!(super::fold_row(cut, turn_id, root)?, Some(row));
        let Some(RootClosure::Abandon { fold, claim }) = root_turn_closure_for_test(cut)? else {
            return Err(format!("{row:?} does not abandon").into());
        };
        assert_eq!(claim, row.claim(), "{row:?}");
        let closed = abandonment_closure_state(cut, fold, claim, cut.id())?;
        let [ContentBlock::Text(text)] = closed
            .history
            .segments()
            .iter()
            .last()
            .ok_or("note")?
            .items()[0]
            .blocks()
        else {
            return Err("note text".into());
        };
        assert_eq!(text.text(), note, "{row:?}");
        let providers = |ledger: &RuntimeOperationLedger| {
            ledger
                .operations()
                .into_iter()
                .filter(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
                .count()
        };
        assert_eq!(
            providers(&closed.ledger),
            providers(cut.manifest().ledger()),
            "{row:?} dispatched"
        );
        for record in cut.manifest().ledger().operations() {
            let workflow = matches!(
                record.kind(),
                DurableOperationKind::Compaction { .. }
                    | DurableOperationKind::Provider {
                        root: ModelOperationRoot::SessionWorkflow(_),
                        ..
                    }
            );
            if !workflow {
                continue;
            }
            let after = closed
                .ledger
                .operations()
                .into_iter()
                .find(|closed| closed.operation_id() == record.operation_id())
                .ok_or("fold record")?;
            if matches!(record.state(), DurableOperationState::Finalized(_)) {
                assert_eq!(after, record, "{row:?} rewrote a committed fold");
            } else {
                assert!(matches!(after.state(), DurableOperationState::Finalized(_)));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn open_admission_fold_row_closes_compaction_not_applied() -> TestResult {
        let cut = with_fold(&root_without_provider_cut(false).await?, false)?;
        assert_row(
            &cut,
            FoldRow::OpenAdmission,
            abandoned_turn_note(
                AbandonedTurnClaim::CompactionNotApplied,
                AbandonmentCause::ProcessLost,
            ),
        )
    }

    #[tokio::test]
    async fn committed_admission_fold_row_names_the_retained_fold() -> TestResult {
        let cut = with_fold(&root_without_provider_cut(false).await?, true)?;
        let row = FoldRow::CommittedAdmission;
        assert_row(
            &cut,
            row,
            row.note(AbandonmentCause::ProcessLost).ok_or("note")?,
        )
    }

    fn with_replay_omission(source: &RuntimeCheckpoint) -> TestResult<RuntimeCheckpoint> {
        let workflow_id = SessionWorkflowRootId::new([0xa5; 32])?;
        let digest = source.manifest().canonical().history().identity();
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&crate::runtime::compaction::DurablePlaceholderEvidence {
                version: 2,
                workflow_id,
                source_checkpoint: source.id(),
                operation: digest,
                successor_history: digest,
                notice_segment: digest,
                replaced_results: 0,
                replaced_bytes: 0,
                after_measurement: digest,
                after_status: digest,
                after_input_tokens: 1,
            })?,
        )?;
        let mut records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let next = records
            .iter()
            .map(DurableOperationRecord::operation_id)
            .max()
            .ok_or("records")?
            + 1;
        records.push(DurableOperationRecord::new(
            next,
            DurableOperationKind::Compaction { workflow_id },
            evidence.clone(),
            vec![DurableOperationTransition::new(
                source.id(),
                DurableOperationState::Finalized(evidence),
            )],
        )?);
        review_tests::checkpoint_with_review_state(
            source,
            records,
            source.manifest().canonical().history().clone(),
            false,
        )
    }

    #[tokio::test]
    async fn committed_replay_omission_admission_cut_retains_fold_without_dispatch() -> TestResult {
        let cut = with_replay_omission(&root_without_provider_cut(false).await?)?;
        let loaded = RuntimeCheckpointCodec::bundle(&cut, None)?.validated_checkpoint()?;
        let row = FoldRow::CommittedAdmission;
        assert_row(
            &loaded,
            row,
            row.note(AbandonmentCause::ProcessLost).ok_or("note")?,
        )
    }

    #[tokio::test]
    async fn committed_replay_omission_overflow_cut_retains_fold_without_dispatch() -> TestResult {
        let source = decided_round(
            &completed_text_round_cut().await?,
            &RoundDecision::OverflowFold,
        )?;
        let cut = with_replay_omission(&source)?;
        let loaded = RuntimeCheckpointCodec::bundle(&cut, None)?.validated_checkpoint()?;
        let row = FoldRow::CommittedOverflow;
        assert_row(
            &loaded,
            row,
            row.note(AbandonmentCause::ProcessLost).ok_or("note")?,
        )
    }

    #[tokio::test]
    async fn committed_overflow_fold_row_names_the_fold_and_does_not_resend() -> TestResult {
        let source = decided_round(
            &completed_text_round_cut().await?,
            &RoundDecision::OverflowFold,
        )?;
        let cut = with_fold(&source, true)?;
        let row = FoldRow::CommittedOverflow;
        let note = row.note(AbandonmentCause::ProcessLost).ok_or("note")?;
        assert!(note.contains("refused request was not sent again"));
        assert!(note.contains("committed compaction remains in history"));
        assert_row(&cut, row, note)
    }

    #[tokio::test]
    async fn open_scheduled_fold_after_tool_batch_names_retained_rounds() -> TestResult {
        let source =
            completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false)
                .await?;
        let row = FoldRow::OpenScheduled;
        let note = row.note(AbandonmentCause::ProcessLost).ok_or("note")?;
        assert!(note.contains("Earlier completed rounds of this turn remain"));
        assert_row(&with_fold(&source, false)?, row, note)
    }

    #[tokio::test]
    async fn open_scheduled_fold_after_continue_names_retained_rounds() -> TestResult {
        let source = decided_round(&completed_text_round_cut().await?, &RoundDecision::Continue)?;
        let row = FoldRow::OpenScheduled;
        assert_row(
            &with_fold(&source, false)?,
            row,
            row.note(AbandonmentCause::ProcessLost).ok_or("note")?,
        )
    }

    #[tokio::test]
    async fn committed_scheduled_fold_after_tool_batch_names_the_fold() -> TestResult {
        let source =
            completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false)
                .await?;
        let row = FoldRow::CommittedScheduled(RoundBoundary::Tool);
        let note = row.note(AbandonmentCause::ProcessLost).ok_or("note")?;
        assert!(note.contains("tool round completed") && note.contains("committed compaction"));
        assert_row(&with_fold(&source, true)?, row, note)
    }

    #[tokio::test]
    async fn committed_scheduled_fold_after_continue_uses_model_round_wording() -> TestResult {
        let source = decided_round(&completed_text_round_cut().await?, &RoundDecision::Continue)?;
        let row = FoldRow::CommittedScheduled(RoundBoundary::Model);
        let note = row.note(AbandonmentCause::ProcessLost).ok_or("note")?;
        assert!(note.contains("model round completed") && note.contains("committed compaction"));
        assert_row(&with_fold(&source, true)?, row, note)
    }
}
