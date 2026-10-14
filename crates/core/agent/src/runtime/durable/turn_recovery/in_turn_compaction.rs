//! Completed paid-fold evidence is not proof that the fold was applied.

use super::{
    AgentError, CheckpointId, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ModelOperationRoot, OperationId, RecoveryAction,
    RecoveryObligation, RecoveryObligationKind, RuntimeOperationLedger, SessionStoreError,
    runtime_manifest_error,
};

pub(super) fn abandonable_debt(
    record: &DurableOperationRecord,
    obligation: &RecoveryObligation,
) -> bool {
    matches!(
        (
            record.kind(),
            obligation.kind(),
            obligation.action(),
            record.state()
        ),
        (
            DurableOperationKind::Compaction { .. },
            RecoveryObligationKind::Compaction,
            RecoveryAction::FinalizationRequired,
            DurableOperationState::Completed(_)
        ) | (
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(_),
                ..
            },
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::FinalizationRequired,
            DurableOperationState::Completed(_)
        )
    ) || matches!(
        (
            record.kind(),
            obligation.kind(),
            obligation.action(),
            record.state()
        ),
        (
            DurableOperationKind::Compaction { .. },
            RecoveryObligationKind::Compaction,
            RecoveryAction::ProviderEvidenceRequired,
            DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_)
        ) | (
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(_),
                ..
            },
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::ProviderEvidenceRequired,
            DurableOperationState::Prepared
                | DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_)
        )
    )
}

pub(super) fn finalize_completed_fold(
    ledger: &RuntimeOperationLedger,
    operation: OperationId,
    checkpoint_id: CheckpointId,
) -> Result<Option<RuntimeOperationLedger>, AgentError> {
    let record = ledger
        .operation(operation.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let DurableOperationState::Completed(evidence) = record.state() else {
        return Ok(None);
    };
    if !matches!(
        record.kind(),
        DurableOperationKind::Compaction { .. }
            | DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(_),
                ..
            }
    ) {
        return Ok(None);
    }
    ledger
        .transition_operation(
            operation.0,
            DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Finalized(evidence.clone()),
            ),
        )
        .map(Some)
        .map_err(runtime_manifest_error)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::runtime::durable::tests::SessionUpdateFixtureMode;
    use crate::runtime::durable::tests::landed_round_abandonment::completed_tool_round_cut;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn compaction_cut(
        source: &RuntimeCheckpoint,
        completed: bool,
    ) -> TestResult<(RuntimeCheckpoint, u64, ReconstructiveComponent)> {
        let mut records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let workflow_id = plakous_model::SessionWorkflowRootId::new([0xa2; 32])?;
        let next = records
            .iter()
            .map(DurableOperationRecord::operation_id)
            .max()
            .ok_or("records")?
            + 1;
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            b"completed summary".to_vec(),
        )?;
        for (operation, kind) in [
            (next, DurableOperationKind::Compaction { workflow_id }),
            (
                next + 1,
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(workflow_id),
                    attempt_id: plakous_model::AttemptId::try_from(uuid::Uuid::from_u128(0xA2_02))?,
                },
            ),
        ] {
            records.push(DurableOperationRecord::new(
                operation,
                kind,
                evidence.clone(),
                [
                    Some(DurableOperationState::Prepared),
                    Some(DurableOperationState::Dispatched),
                    completed.then(|| DurableOperationState::Completed(evidence.clone())),
                ]
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, state)| {
                    Ok(DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xA2_10 + index as u128))?,
                        state,
                    ))
                })
                .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?,
            )?);
        }
        let cut = review_tests::checkpoint_with_review_state(
            source,
            records,
            source.manifest().canonical().history().clone(),
            true,
        )?;
        Ok((cut, next, evidence))
    }

    #[tokio::test]
    async fn completed_in_turn_compaction_abandons_without_applying_or_replaying() -> TestResult {
        let source =
            completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false)
                .await?;
        let (cut, next, evidence) = compaction_cut(&source, true)?;
        let fold = abandoned_turn_recovery_fold(&cut)?;
        assert_eq!(
            abandoned_turn_claim(&fold),
            AbandonedTurnClaim::CompactionNotApplied
        );
        let closed = abandonment_closure_state(
            &cut,
            fold,
            AbandonedTurnClaim::CompactionNotApplied,
            source.id(),
        )?;
        let note = serde_json::to_string(closed.history.segments().iter().last().ok_or("note")?)?;
        assert!(
            note.contains("Earlier completed rounds of this turn remain in this history"),
            "{note}"
        );
        assert!(!note.contains("No response from this turn"), "{note}");
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
        assert_eq!(
            text.text(),
            "this turn was abandoned after the process was lost while the conversation was being compacted. Compaction was not applied and may have been billed. Earlier completed rounds of this turn remain in this history; the history before this point is unchanged."
        );
        for operation in [next, next + 1] {
            assert!(closed.ledger.operations().into_iter().any(|record| record.operation_id() == operation
                && matches!(record.state(), DurableOperationState::Finalized(actual) if actual == &evidence)));
        }
        assert_eq!(
            closed.history.segments().len(),
            cut.manifest().canonical().history().segments().len() + 1
        );
        Ok(())
    }
    #[tokio::test]
    async fn a2_finalized_failed_attempt_keeps_original_compaction_note() -> TestResult {
        let source =
            crate::runtime::durable::tests::landed_round_abandonment::completed_text_round_cut()
                .await?;
        let records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|record| {
                if !matches!(record.kind(), DurableOperationKind::Provider { .. }) {
                    return Ok(record.clone());
                }
                let evidence =
                    crate::runtime::operation_record::test_support::outcome_component(Err(
                        &AgentError::new(AgentErrorKind::Model, "provider attempt failed"),
                    ))?;
                let mut transitions = record.transitions().to_vec();
                transitions.pop();
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
        let source = review_tests::checkpoint_with_review_state(
            &source,
            records,
            source.manifest().canonical().history().clone(),
            false,
        )?;
        let (cut, _, _) = compaction_cut(&source, false)?;
        let fold = abandoned_turn_recovery_fold(&cut)?;
        let closed = abandonment_closure_state(
            &cut,
            fold,
            AbandonedTurnClaim::CompactionNotApplied,
            cut.id(),
        )?;
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
        assert_eq!(
            text.text(),
            abandoned_turn_note(
                AbandonedTurnClaim::CompactionNotApplied,
                AbandonmentCause::ProcessLost
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn a2_unapplied_compaction_without_prior_round_keeps_original_closure_note() -> TestResult
    {
        let source =
            crate::runtime::durable::tests::landed_round_abandonment::root_without_provider_cut(
                false,
            )
            .await?;
        let (cut, _, _) = compaction_cut(&source, false)?;
        let fold = abandoned_turn_recovery_fold(&cut)?;
        assert_eq!(
            abandoned_turn_claim(&fold),
            AbandonedTurnClaim::CompactionNotApplied
        );
        let closed = abandonment_closure_state(
            &cut,
            fold,
            AbandonedTurnClaim::CompactionNotApplied,
            cut.id(),
        )?;
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
        assert_eq!(
            text.text(),
            "this turn was abandoned after the process was lost while the conversation was being compacted. Compaction was not applied; the history before this point is unchanged. The compaction request may have been billed. No response from this turn is part of this history."
        );
        Ok(())
    }
}
