//! Admission can commit a paid fold before the turn's first provider request
//! (the fold rows, `fold_rows.rs`).

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::runtime::durable::tests::landed_round_abandonment::root_without_provider_cut;
    use plakous_model::{AttemptId, CompactionBlock, SessionWorkflowRootId};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn admission_compaction_committed_fold_survives_root_abandonment() -> TestResult {
        committed_fold_note(0).await
    }

    #[tokio::test]
    async fn admission_compaction_older_fold_keeps_nothing_ran_note() -> TestResult {
        committed_fold_note(3).await
    }

    #[tokio::test]
    async fn admission_compaction_only_provider_before_root_keeps_nothing_ran_note() -> TestResult {
        committed_fold_note(1).await
    }

    #[tokio::test]
    async fn admission_compaction_only_compaction_before_root_keeps_nothing_ran_note() -> TestResult
    {
        committed_fold_note(2).await
    }

    async fn committed_fold_note(older: u8) -> TestResult {
        let source = root_without_provider_cut(false).await?;
        let mut records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let workflow = SessionWorkflowRootId::new([0xa3; 32])?;
        let digest = source.manifest().canonical().history().identity();
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "workflow_id": workflow,
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
            .ok_or("root")?
            + 1;
        for (operation, kind) in [
            (
                next,
                DurableOperationKind::Compaction {
                    workflow_id: workflow,
                },
            ),
            (
                next + 1,
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(workflow),
                    attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xa3_01))?,
                },
            ),
        ] {
            records.push(DurableOperationRecord::new(
                operation,
                kind,
                evidence.clone(),
                [
                    DurableOperationState::Prepared,
                    DurableOperationState::Dispatched,
                    DurableOperationState::Completed(evidence.clone()),
                    DurableOperationState::Finalized(evidence.clone()),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, state)| {
                    Ok(DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xa3_10 + index as u128))?,
                        state,
                    ))
                })
                .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?,
            )?);
        }
        let summary = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(0xa3_20))?,
                Actor::Assistant,
                vec![ContentBlock::Compaction(CompactionBlock::new(
                    "committed admission summary",
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let history = source
            .manifest()
            .canonical()
            .history()
            .clone()
            .append(summary.clone())?;
        if older != 0 {
            let root_id = match older {
                1 | 2 => next,
                _ => next + 2,
            };
            let root = &records[0];
            records[0] = DurableOperationRecord::new(
                root_id,
                root.kind().clone(),
                root.request().clone(),
                root.transitions().to_vec(),
            )?;
            if older < 3 {
                let index = if older == 1 { 2 } else { 1 };
                let record = &records[index];
                records[index] = DurableOperationRecord::new(
                    next - 1,
                    record.kind().clone(),
                    record.request().clone(),
                    record.transitions().to_vec(),
                )?;
                if older == 1 {
                    let compaction = &records[1];
                    records[1] = DurableOperationRecord::new(
                        next + 1,
                        compaction.kind().clone(),
                        compaction.request().clone(),
                        compaction.transitions().to_vec(),
                    )?;
                }
            }
            records.sort_by_key(DurableOperationRecord::operation_id);
        }
        let cut =
            review_tests::checkpoint_with_review_state(&source, records.clone(), history, false)?;
        let fold = abandoned_turn_recovery_fold(&cut)?;
        assert_eq!(fold.root, AbandonedRoot::Dispatched);
        assert_eq!(
            abandoned_turn_claim(&fold),
            AbandonedTurnClaim::NothingStarted
        );
        assert!(fold.provider_operations.is_empty());
        assert!(
            fold.compaction_operations.is_empty(),
            "a committed fold is not abandoned debt"
        );
        let closed =
            abandonment_closure_state(&cut, fold, AbandonedTurnClaim::NothingStarted, source.id())?;
        assert_eq!(
            closed.history.segments().len(),
            cut.manifest().canonical().history().segments().len() + 1
        );
        assert!(
            closed
                .history
                .segments()
                .iter()
                .zip(cut.manifest().canonical().history().segments().iter())
                .all(|(actual, original)| actual == original)
        );
        assert!(
            closed
                .history
                .segments()
                .iter()
                .any(|segment| segment == &summary)
        );
        for original in records.iter().filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Compaction { .. } | DurableOperationKind::Provider { .. }
            )
        }) {
            assert!(closed.ledger.operations().contains(&original));
        }
        let note = closed
            .history
            .segments()
            .iter()
            .last()
            .ok_or("note")?
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text()),
                _ => None,
            })
            .ok_or("text")?;
        if older != 0 {
            assert!(note.contains("Nothing ran"), "{note}");
            assert!(!note.contains("committed compaction"), "{note}");
            return Ok(());
        }
        assert!(note.contains("committed compaction"), "{note}");
        assert!(
            !note.contains("Nothing ran"),
            "the summary provider already ran: {note}"
        );
        assert!(
            !note.contains("not applied"),
            "the fold was committed: {note}"
        );
        Ok(())
    }
}
