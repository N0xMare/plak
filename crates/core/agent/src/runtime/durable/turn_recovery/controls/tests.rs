#![allow(
    clippy::too_many_lines,
    reason = "Control recovery scenarios correlate steering, cancellation, and command acknowledgements with the folded checkpoint"
)]

use super::super::*;
use super::{STEERING_NOT_APPLIED, has_unapplied_steering};
use crate::TurnCommand;
use crate::runtime::durable::tests::landed_round_abandonment::{
    completed_text_round_cut, root_without_provider_cut,
};
use crate::runtime::terminal::decode_terminal_result_record;
use crate::runtime::{DurableActorCommandEffect, DurableCommandEnvelope};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

pub(in crate::runtime::durable::turn_recovery) fn with_control(
    source: &RuntimeCheckpoint,
    effect: DurableActorCommandEffect,
    transition_count: usize,
) -> TestResult<RuntimeCheckpoint> {
    let turn_id = last_root_turn(source.manifest().ledger())?;
    let command = TurnCommand::Steer {
        command_id: CommandId::new("controls:steer")?,
        turn_id,
        input: "queued steering".into(),
    };
    let component = runtime_component(
        RuntimeComponentKind::Command,
        serde_json::to_vec(&DurableCommandEnvelope {
            version: 1,
            canonical_input: crate::runtime::control::command_canonical_bytes(&command),
            effect,
            rejection: None,
        })?,
    )?;
    let mut commands = source
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    commands.push(DurableCommandRecord::new(
        command.command_id().as_str(),
        commands.len() as u64 + 1,
        Some(turn_id),
        crate::runtime::control::command_binding(&command)?,
        component.clone(),
        DurableCommandDisposition::Accepted,
        None,
    )?);
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
    let evidence = runtime_component(
        RuntimeComponentKind::Operation,
        b"steering-applied".to_vec(),
    )?;
    let mut states = vec![
        DurableOperationState::Prepared,
        DurableOperationState::Dispatched,
    ];
    {
        states.extend([
            DurableOperationState::Completed(evidence.clone()),
            DurableOperationState::Finalized(evidence),
        ]);
    }
    states.truncate(transition_count);
    records.push(DurableOperationRecord::new(
        next,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        runtime_component(RuntimeComponentKind::Operation, component.bytes().to_vec())?,
        states
            .into_iter()
            .enumerate()
            .map(|(index, state)| {
                Ok(DurableOperationTransition::new(
                    CheckpointId::try_from(uuid::Uuid::from_u128(0xA2E_100 + index as u128))?,
                    state,
                ))
            })
            .collect::<TestResult<Vec<_>>>()?,
    )?);
    review_tests::checkpoint_with_review_commands(
        source,
        commands,
        records,
        source.manifest().canonical().history().clone(),
        false,
    )
}

#[tokio::test]
async fn recovery_controls_do_not_block_root_only_abandonment() -> TestResult {
    for prepared in [false, true] {
        for effect in [
            DurableActorCommandEffect::SteerQueued { position: 1 },
            DurableActorCommandEffect::CancelRequested,
            DurableActorCommandEffect::CancelAlreadyRequested,
            DurableActorCommandEffect::CancelAlreadyTerminal,
            DurableActorCommandEffect::EffectDecisionAllowed,
            DurableActorCommandEffect::EffectDecisionDenied,
            DurableActorCommandEffect::Shutdown,
        ] {
            for transition_count in 1..=4 {
                let cut = with_control(
                    &root_without_provider_cut(prepared).await?,
                    effect.clone(),
                    transition_count,
                )?;
                let fold = abandoned_turn_recovery_fold(&cut)?;
                assert_eq!(
                    abandoned_turn_claim(&fold),
                    AbandonedTurnClaim::NothingStarted
                );
                let closed = abandonment_closure_state(
                    &cut,
                    fold,
                    AbandonedTurnClaim::NothingStarted,
                    cut.id(),
                )?;
                if transition_count == 3 {
                    let prior = cut
                        .manifest()
                        .ledger()
                        .operations()
                        .into_iter()
                        .max_by_key(|record| record.operation_id())
                        .ok_or("control")?;
                    let DurableOperationState::Completed(completed) = prior.state() else {
                        return Err("completed control".into());
                    };
                    let control = closed
                        .ledger
                        .operation(prior.operation_id())
                        .ok_or("closed control")?;
                    assert!(
                        matches!(control.state(), DurableOperationState::Finalized(evidence)
                        if evidence.bytes() == if matches!(effect, DurableActorCommandEffect::SteerQueued { .. }) {
                            STEERING_NOT_APPLIED
                        } else { completed.bytes() })
                    );
                }
                assert!(matches!(
                    working_lifecycle(last_root_turn(&closed.ledger)?, &closed.ledger)?,
                    RuntimeLifecycle::Quiescent
                ));
                let note =
                    serde_json::to_string(closed.history.segments().iter().last().ok_or("note")?)?;
                assert!(
                    note.contains("before any model request or tool started"),
                    "{note}"
                );
                assert_eq!(
                    note.matches("Queued steering was not applied and is not retried")
                        .count(),
                    usize::from(
                        matches!(effect, DurableActorCommandEffect::SteerQueued { .. })
                            && transition_count <= 3
                    ),
                    "{note}"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_completed_newest_provider_settles_queued_steer_unapplied() -> TestResult {
    for transition_count in 1..=4 {
        let cut = with_control(
            &completed_text_round_cut().await?,
            DurableActorCommandEffect::SteerQueued { position: 1 },
            transition_count,
        )?;
        let fold = local_terminal_recovery_fold(&cut)?;
        let closed = terminal_closure_state(&cut, fold, false, cut.id())?;
        assert!(matches!(
            working_lifecycle(last_root_turn(&closed.ledger)?, &closed.ledger)?,
            RuntimeLifecycle::Quiescent
        ));
        let steer = closed
            .ledger
            .operations()
            .into_iter()
            .max_by_key(|record| record.operation_id())
            .ok_or("steer")?;
        assert!(
            matches!(steer.state(), DurableOperationState::Finalized(evidence) if evidence.bytes() == if transition_count >= 4 { &b"steering-applied"[..] } else { STEERING_NOT_APPLIED })
        );
        let terminals = closed
            .ledger
            .operations()
            .into_iter()
            .filter_map(|record| match record.state() {
                DurableOperationState::Finalized(evidence) => {
                    decode_terminal_result_record(evidence.bytes())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert!(matches!(
            terminals[0].outcome,
            DurableTerminalOutcome::Completed(_)
        ));
        assert_eq!(
            has_unapplied_steering(&closed.ledger, last_root_turn(&closed.ledger)?),
            transition_count <= 3
        );
        let checkpoint = review_tests::checkpoint_with_review_commands(
            &cut,
            closed.ledger.commands().into_iter().cloned().collect(),
            closed.ledger.operations().into_iter().cloned().collect(),
            closed.history,
            false,
        )?;
        let result = crate::runtime::terminal::checkpoint_root_terminal_result(
            &Arc::new(checkpoint),
            &closed.command_id,
        )?
        .ok_or("terminal result")?;
        assert_eq!(result.has_unapplied_steering(), transition_count <= 3);
    }
    Ok(())
}

#[tokio::test]
async fn recovery_finalized_model_round_with_steer_abandons_at_round_boundary() -> TestResult {
    for transition_count in 1..=4 {
        let source = completed_text_round_cut().await?;
        let response = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find_map(|record| {
                if let (
                    DurableOperationKind::Provider { .. },
                    DurableOperationState::Completed(evidence),
                ) = (record.kind(), record.state())
                {
                    crate::runtime::operation_record::test_support::recorded_response_parts(
                        evidence,
                    )
                    .ok()
                    .map(|(_, response)| response)
                } else {
                    None
                }
            })
            .ok_or("response")?;
        let history =
            source
                .manifest()
                .canonical()
                .history()
                .clone()
                .append(HistorySegment::new(
                    history_segment_kind(response.output()),
                    response.output().to_vec(),
                )?)?;
        let records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|record| {
                let mut transitions = record.transitions().to_vec();
                if matches!(record.kind(), DurableOperationKind::Provider { .. }) {
                    let DurableOperationState::Completed(evidence) = record.state() else {
                        return Err("completed provider".into());
                    };
                    // The live driver promotes an assistant continuation
                    // under a `Continue` decision.
                    let continued = runtime_component(
                        RuntimeComponentKind::Operation,
                        crate::runtime::operation_record::test_support::recorded_decision_bytes(
                            evidence,
                            RoundDecision::Continue,
                        )?,
                    )?;
                    let last = transitions.len() - 1;
                    transitions[last] = DurableOperationTransition::new(
                        transitions[last].checkpoint_id(),
                        DurableOperationState::Completed(continued.clone()),
                    );
                    transitions.push(DurableOperationTransition::new(
                        source.id(),
                        DurableOperationState::Finalized(continued),
                    ));
                }
                Ok(DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    transitions,
                )?)
            })
            .collect::<TestResult<Vec<_>>>()?;
        let source = review_tests::checkpoint_with_review_state(&source, records, history, false)?;
        let cut = with_control(
            &source,
            DurableActorCommandEffect::SteerQueued { position: 1 },
            transition_count,
        )?;
        let fold = abandoned_turn_recovery_fold(&cut)?;
        assert!(fold.tool_round_completed);
        assert_eq!(
            abandoned_turn_claim(&fold),
            AbandonedTurnClaim::ToolRoundCompleted
        );
        let closed = abandonment_closure_state(
            &cut,
            fold,
            AbandonedTurnClaim::ToolRoundCompleted,
            cut.id(),
        )?;
        let note = serde_json::to_string(closed.history.segments().iter().last().ok_or("note")?)?;
        assert!(note.contains("model round completed"), "{note}");
        assert!(!note.contains("tool results"), "{note}");
        if transition_count <= 3 {
            assert!(
                note.contains("Queued steering was not applied and is not retried"),
                "{note}"
            );
            assert!(has_unapplied_steering(
                &closed.ledger,
                last_root_turn(&closed.ledger)?
            ));
            assert_eq!(
                note.matches("Queued steering was not applied and is not retried")
                    .count(),
                1
            );
        } else {
            assert!(!note.contains("Queued steering was not applied"));
        }
    }
    Ok(())
}
