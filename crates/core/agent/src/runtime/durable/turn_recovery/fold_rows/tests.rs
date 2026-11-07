use super::super::abandonment::abandoned_turn_note;
use super::super::*;
use super::FoldRow;
use super::replay::RoundBoundary;
use crate::RuntimeCheckpointCodec;
use crate::runtime::durable::tests::SessionUpdateFixtureMode;
use crate::runtime::durable::tests::landed_round_abandonment::{
    completed_text_round_cut, completed_tool_round_cut, root_without_provider_cut,
};
use crate::runtime::operation_record::test_support::{outcome_component, recorded_decision_bytes};
use plakous_model::{AttemptId, CanonicalHistory, CompactionBlock, SessionWorkflowRootId};

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
            .operation(record.operation_id())
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
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false).await?;
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
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false).await?;
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

fn retirement_checkpoint(
    ledger: RuntimeOperationLedger,
    turn: TurnId,
    history: CanonicalHistory,
) -> TestResult<RuntimeCheckpoint> {
    let lifecycle = working_lifecycle(turn, &ledger)?;
    crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        history,
        3,
        true,
        Some(ledger),
        Some(lifecycle),
    )
}

#[test]
fn retired_closed_cells_count_dispatched_waits_but_not_child_edges() -> TestResult {
    use crate::runtime::operation_record::{retire_test_ledger, retirement_fixture};
    let (ledger, turn) = retirement_fixture(200, RoundDecision::Continue)?;
    let mut records = ledger
        .operations()
        .into_iter()
        .filter(|record| !(101..=103).contains(&record.operation_id()))
        .cloned()
        .collect::<Vec<_>>();
    for record in crate::manifest::composition_graph_records()? {
        let mut kind = record.kind().clone();
        match &mut kind {
            DurableOperationKind::CompositionEvaluation { turn_id, .. }
            | DurableOperationKind::CompositionContinuation { turn_id, .. }
            | DurableOperationKind::CompositionChild { turn_id, .. } => *turn_id = turn,
            _ => return Err("composition fixture kind".into()),
        }
        records.push(DurableOperationRecord::new(
            record.operation_id(),
            kind,
            record.request().clone(),
            record.transitions().to_vec(),
        )?);
    }
    records.sort_by_key(DurableOperationRecord::operation_id);
    let full =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    let retained = retire_test_ledger(&full, turn, 16)?;
    for id in 101..=103 {
        assert!(retained.operation(id).is_none(), "closed group {id}");
    }
    let before = terminal_turn_usage(&full, turn)?;
    assert_eq!(before.local_tool_attempts(), 199);
    assert_eq!(terminal_turn_usage(&retained, turn)?, before);
    Ok(())
}

#[test]
fn retirement_preserves_open_scheduled_committed_scheduled_and_overflow_rows() -> TestResult {
    use crate::runtime::operation_record::{retire_test_ledger, retirement_fixture};
    for (decision, committed, expected) in [
        (RoundDecision::Continue, false, FoldRow::OpenScheduled),
        (
            RoundDecision::Continue,
            true,
            FoldRow::CommittedScheduled(RoundBoundary::Model),
        ),
        (
            RoundDecision::OverflowFold,
            true,
            FoldRow::CommittedOverflow,
        ),
    ] {
        let (ledger, turn) = retirement_fixture(100, decision)?;
        let source = retirement_checkpoint(ledger, turn, CanonicalHistory::default())?;
        let source = with_fold(&source, committed)?;
        assert_eq!(
            super::fold_row(&source, turn, OperationId(1))?,
            Some(expected)
        );
        let retained = retire_test_ledger(source.manifest().ledger(), turn, 16)?;
        assert!(retained.operation_count() < source.manifest().ledger().operation_count());
        let after = retirement_checkpoint(
            retained,
            turn,
            source.manifest().canonical().history().clone(),
        )?;
        assert_eq!(
            super::fold_row(&after, turn, OperationId(1))?,
            Some(expected)
        );
    }
    Ok(())
}

#[test]
fn retirement_preserves_unsent_fold_retry_and_retained_round_witnesses() -> TestResult {
    use crate::runtime::operation_record::{
        retire_test_ledger, retirement_fixture, retirement_round,
    };
    for (decision, expected) in [
        (RoundDecision::OverflowFold, replay::UnsentDecision::Fold),
        (
            RoundDecision::RetryOutputLimit,
            replay::UnsentDecision::Retry,
        ),
    ] {
        let (ledger, turn) = retirement_fixture(100, decision)?;
        assert_eq!(
            replay::unsent_decision(&ledger, turn, OperationId(1))?,
            Some(expected)
        );
        let retained = retire_test_ledger(&ledger, turn, 16)?;
        assert!(retained.operation_count() < ledger.operation_count());
        assert_eq!(
            replay::unsent_decision(&retained, turn, OperationId(1))?,
            Some(expected)
        );
    }
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let ledger = ledger.append_operation(retirement_round(
        103,
        turn,
        RoundDecision::RetryOutputLimit,
    )?)?;
    let retained = retire_test_ledger(&ledger, turn, 16)?;
    assert!(replay::retained_round(
        retained.operation(102).ok_or("earlier round")?
    )?);
    Ok(())
}

#[tokio::test]
async fn retirement_respects_closed_baton_references_reaches_target_and_matches_its_twin()
-> TestResult {
    use crate::runtime::operation_record::{retire_test_ledger, retirement_leaf, retirement_round};
    let source = crate::runtime::durable::tests::landed_round_abandonment::completed_tool_round_cut_with_lineage(
        SessionUpdateFixtureMode::JoinedForeground, false, false, true,
    ).await?;
    let (turn, root) = turn_and_root(&source)?;
    let (component, attempt, effect) = completed_batch_frame(&source)?;
    let mut records = finalized_batch_records(&source)?;
    let mut next = records.last().ok_or("last operation")?.operation_id() + 1;
    for _ in 0..100 {
        records.push(retirement_leaf(next, turn)?);
        next += 1;
    }
    let baton = next;
    records.push(DurableOperationRecord::new(
        baton,
        DurableOperationKind::ToolBatchContinuation {
            turn_id: turn,
            attempt_id: attempt,
        },
        component.clone(),
        vec![
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0x9901))?,
                DurableOperationState::ContinuationRequired(component.clone()),
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0x9902))?,
                DurableOperationState::Finalized(component),
            ),
        ],
    )?);
    records.push(retirement_round(baton + 1, turn, RoundDecision::Continue)?);
    let full = RuntimeOperationLedger::new(
        source
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .cloned()
            .collect(),
        records,
    )?;
    let retained = retire_test_ledger(&full, turn, 16)?;
    assert!(retained.operation_count() <= 12);
    assert!(retained.operation(baton).is_some());
    assert_dependency_publication_refusal(&full, turn, effect)?;
    assert_eq!(retained.operation(effect), full.operation(effect));
    assert_eq!(
        terminal_turn_usage(&retained, turn)?,
        terminal_turn_usage(&full, turn)?
    );
    let before =
        retirement_checkpoint(full, turn, source.manifest().canonical().history().clone())?;
    let after = retirement_checkpoint(
        retained,
        turn,
        source.manifest().canonical().history().clone(),
    )?;
    assert_eq!(
        super::fold_row(&before, turn, root)?,
        super::fold_row(&after, turn, root)?
    );
    let close = |checkpoint: &RuntimeCheckpoint| -> TestResult<_> {
        let Some(RootClosure::Abandon { fold, claim }) = root_turn_closure_for_test(checkpoint)?
        else {
            return Err("abandonment twin".into());
        };
        Ok(abandonment_closure_state(
            checkpoint,
            fold,
            claim,
            checkpoint.id(),
        )?)
    };
    let before = close(&before)?;
    let after = close(&after)?;
    assert_eq!(before.history.identity(), after.history.identity());
    assert_eq!(
        before.ledger.operation(root.0),
        after.ledger.operation(root.0)
    );
    Ok(())
}

fn finalized_batch_records(source: &RuntimeCheckpoint) -> TestResult<Vec<DurableOperationRecord>> {
    source
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .map(|record| {
            let mut transitions = record.transitions().to_vec();
            if let DurableOperationState::ContinuationRequired(evidence) = record.state() {
                transitions.push(DurableOperationTransition::new(
                    CheckpointId::try_from(uuid::Uuid::from_u128(0x9900))?,
                    DurableOperationState::Finalized(evidence.clone()),
                ));
            }
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                transitions,
            )?)
        })
        .collect()
}

fn assert_dependency_publication_refusal(
    full: &RuntimeOperationLedger,
    turn: TurnId,
    effect: u64,
) -> TestResult {
    use crate::runtime::checkpoint::candidate::{SuccessorFacts, successor_candidate};
    let parent = retirement_checkpoint(full.clone(), turn, CanonicalHistory::default())?;
    let forged = RuntimeOperationLedger::new(
        full.commands().into_iter().cloned().collect(),
        full.operations()
            .into_iter()
            .filter(|record| record.operation_id() != effect)
            .cloned()
            .collect(),
    )?
    .with_retired_attempts(Some((turn, 1)))?
    .with_retirement_source(full, &std::collections::BTreeSet::new());
    let manifest = parent.manifest();
    let result = successor_candidate(
        &parent,
        &RuntimeCheckpointCodec::bundle(&parent, None)?,
        SuccessorFacts {
            checkpoint_id: CheckpointId::try_from(uuid::Uuid::from_u128(0x9910))?,
            lifecycle: manifest.lifecycle().clone(),
            history: manifest.canonical().history().clone(),
            ledger: forged,
            jobs: manifest.jobs().clone(),
            named_state: manifest.named_state().clone(),
            context: manifest.canonical().context().retained_for_exact_resume(),
            family: manifest.family_artifact().cloned(),
        },
        1,
        None,
    );
    let error = result
        .err()
        .ok_or("frame dependency was lost before publication")?;
    assert_eq!(error.kind(), AgentErrorKind::Internal);
    assert_eq!(
        error.message(),
        "operation ledger retirement violates its frontier or accounting"
    );
    Ok(())
}

fn completed_batch_frame(
    source: &RuntimeCheckpoint,
) -> TestResult<(ReconstructiveComponent, AttemptId, u64)> {
    use crate::runtime::continuation::{ContinuationToolSlot, RuntimeContinuation};
    let ledger = source.manifest().ledger();
    let record = ledger
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::ToolBatchContinuation { .. }
            )
        })
        .ok_or("batch frame")?;
    let RuntimeContinuation::AwaitToolBatchV2(mut frame) =
        serde_json::from_slice(record.request().bytes())?
    else {
        return Err("tool batch".into());
    };
    let call = frame
        .provider_response
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .ok_or("tool call")?;
    let effect = ledger.operations().into_iter().find(|record| matches!(record.kind(), DurableOperationKind::Effect { call_id, .. } if *call_id == call.call_id())).ok_or("effect")?.operation_id();
    let item = source.manifest().canonical().history().segments().iter().flat_map(HistorySegment::items).find(|item| item.blocks().iter().any(|block| matches!(block, ContentBlock::ToolResult(result) if result.call_id() == call.call_id()))).ok_or("committed result")?.clone();
    frame.slots = vec![ContinuationToolSlot::Completed {
        call,
        result_entry_id: item.id(),
        item,
        operation_id: OperationId(effect),
    }];
    let attempt = frame.attempt_id;
    Ok((
        runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&RuntimeContinuation::AwaitToolBatchV2(frame))?,
        )?,
        attempt,
        effect,
    ))
}
