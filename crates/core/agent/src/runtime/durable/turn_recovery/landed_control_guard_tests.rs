use super::controls::tests::with_control;
use super::*;
use crate::runtime::durable::tests::landed_round_abandonment::{
    completed_text_round_cut, root_without_provider_cut,
};
use crate::runtime::{DurableActorCommandEffect, DurableCommandEnvelope, DurableControlRejection};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn landed_provider_refuses_pending_nonsteering_controls() -> TestResult {
    for effect in [
        DurableActorCommandEffect::CancelRequested,
        DurableActorCommandEffect::EffectDecisionAllowed,
        DurableActorCommandEffect::EffectDecisionDenied,
    ] {
        let cut = with_control(&completed_text_round_cut().await?, effect, 2)?;
        let before = cut.clone();
        assert_eq!(
            local_terminal_recovery_fold(&cut)
                .err()
                .ok_or("refusal")?
                .kind(),
            AgentErrorKind::OutcomeUnknown
        );
        assert_eq!(cut, before);
    }
    Ok(())
}

#[tokio::test]
async fn same_turn_control_refusal_matrix_leaves_head_unchanged() -> TestResult {
    let source = with_control(
        &root_without_provider_cut(false).await?,
        DurableActorCommandEffect::SteerQueued { position: 1 },
        1,
    )?;
    let turn = last_root_turn(source.manifest().ledger())?;
    let other = TurnId::try_from(uuid::Uuid::from_u128(0xA2_888))?;
    for case in 0..8 {
        let ledger = source.manifest().ledger();
        let control = ledger.commands().into_iter().last().ok_or("command")?;
        let operation = ledger.operations().into_iter().last().ok_or("operation")?;
        let mut envelope: DurableCommandEnvelope =
            serde_json::from_slice(control.command_component().bytes())?;
        match case {
            4 => {
                envelope.rejection = Some(DurableControlRejection {
                    kind: "wrong_phase".into(),
                    message: "rejected".into(),
                });
            }
            5 => envelope.effect = DurableActorCommandEffect::SteerQueued { position: 0 },
            6 => envelope.effect = DurableActorCommandEffect::Root,
            7 => envelope.version = 2,
            _ => {}
        }
        let bytes = serde_json::to_vec(&envelope)?;
        let mut commands = ledger.commands().into_iter().cloned().collect::<Vec<_>>();
        commands.pop();
        commands.push(DurableCommandRecord::new(
            control.command_id(),
            control.sequence(),
            match case {
                1 => None,
                2 => Some(other),
                _ => Some(turn),
            },
            control.binding(),
            runtime_component(RuntimeComponentKind::Command, bytes.clone())?,
            if case == 3 {
                DurableCommandDisposition::Rejected
            } else {
                DurableCommandDisposition::Accepted
            },
            (case == 3).then(|| "wrong_phase".to_owned()),
        )?);
        let mut records = ledger.operations().into_iter().cloned().collect::<Vec<_>>();
        records.pop();
        records.push(DurableOperationRecord::new(
            operation.operation_id(),
            DurableOperationKind::Command {
                turn_id: Some(if case == 0 { other } else { turn }),
            },
            runtime_component(RuntimeComponentKind::Operation, bytes)?,
            operation.transitions().to_vec(),
        )?);
        let cut = review_tests::checkpoint_with_review_commands(
            &source,
            commands,
            records,
            source.manifest().canonical().history().clone(),
            false,
        )?;
        let before = cut.clone();
        assert!(
            !controls::same_turn_control(
                cut.manifest().ledger(),
                cut.manifest()
                    .ledger()
                    .operations()
                    .into_iter()
                    .last()
                    .ok_or("control")?,
                turn
            ),
            "case {case}"
        );
        assert_eq!(
            abandoned_turn_recovery_fold(&cut)
                .err()
                .ok_or("refusal")?
                .kind(),
            AgentErrorKind::OutcomeUnknown,
            "case {case}"
        );
        assert_eq!(cut, before);
    }
    Ok(())
}

#[tokio::test]
async fn recorded_terminal_fallback_finalizes_proven_unrun_control() -> TestResult {
    let cut = root_without_provider_cut(false).await?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    let id = CheckpointId::try_from(uuid::Uuid::from_u128(0xA2_900))?;
    let closed = abandonment_closure_state(&cut, fold, AbandonedTurnClaim::NothingStarted, id)?;
    let controlled = with_control(
        &cut,
        DurableActorCommandEffect::SteerQueued { position: 1 },
        1,
    )?;
    let mut records = controlled
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let control = records.pop().ok_or("control")?;
    records[0] = closed.ledger.operations()[0].clone();
    let proof = runtime_component(
        RuntimeComponentKind::Operation,
        controls::STEERING_NOT_APPLIED.to_vec(),
    )?;
    let mut transitions = control.transitions().to_vec();
    transitions.push(DurableOperationTransition::new(
        id,
        DurableOperationState::ProvenNotDispatched(proof),
    ));
    records.push(DurableOperationRecord::new(
        control.operation_id(),
        control.kind().clone(),
        control.request().clone(),
        transitions,
    )?);
    let cut = review_tests::checkpoint_with_review_commands(
        &cut,
        controlled
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .cloned()
            .collect(),
        records,
        closed.history,
        false,
    )?;
    let fold = local_terminal_recovery_fold(&cut)?;
    let closed = terminal_closure_state(&cut, fold, false, cut.id())?;
    assert!(controls::has_unapplied_steering(
        &closed.ledger,
        last_root_turn(&closed.ledger)?
    ));
    assert!(matches!(
        working_lifecycle(last_root_turn(&closed.ledger)?, &closed.ledger)?,
        RuntimeLifecycle::Quiescent
    ));
    Ok(())
}

#[tokio::test]
async fn real_tool_round_with_pending_control_keeps_tool_boundary() -> TestResult {
    use crate::runtime::durable::tests::{
        SessionUpdateFixtureMode, landed_round_abandonment::completed_tool_round_cut,
    };
    let source =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false).await?;
    let cut = with_control(
        &source,
        DurableActorCommandEffect::SteerQueued { position: 1 },
        2,
    )?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    assert!(fold.tool_round_completed);
    assert_eq!(
        abandoned_turn_claim(&fold),
        AbandonedTurnClaim::ToolRoundCompleted
    );
    let closed =
        abandonment_closure_state(&cut, fold, AbandonedTurnClaim::ToolRoundCompleted, cut.id())?;
    let note = serde_json::to_string(closed.history.segments().iter().last().ok_or("note")?)?;
    assert!(note.contains("completed tool results remain"), "{note}");
    assert!(
        note.contains("Queued steering was not applied and is not retried"),
        "{note}"
    );
    Ok(())
}

#[tokio::test]
async fn rev_probe_identical_earlier_steer_does_not_prove_application() -> TestResult {
    let source = with_control(
        &root_without_provider_cut(false).await?,
        DurableActorCommandEffect::SteerQueued { position: 1 },
        3,
    )?;
    for prior_turn in [true, false] {
        let item = ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(0xA2_777))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("queued steering", None)?)],
            Provenance::UserInput,
        )?;
        let segment = HistorySegment::new(HistorySegmentKind::Steering, vec![item])?;
        let mut segments = source
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        if prior_turn {
            segments.insert(0, segment);
        } else {
            segments.push(segment);
        }
        let cut = review_tests::checkpoint_with_review_state(
            &source,
            source
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .cloned()
                .collect(),
            plakous_model::CanonicalHistory::new(segments)?,
            false,
        )?;
        let ledger = cut.manifest().ledger();
        let control = ledger.operations().into_iter().last().ok_or("control")?;
        let closed = controls::settle_control(ledger, control, cut.id())?;
        assert!(
            controls::has_unapplied_steering(&closed, last_root_turn(ledger)?),
            "identical earlier steer, prior_turn={prior_turn}"
        );
    }
    Ok(())
}

#[test]
fn compaction_without_completed_rounds_keeps_original_note() {
    assert_eq!(
        abandoned_turn_note(
            AbandonedTurnClaim::CompactionNotApplied,
            AbandonmentCause::ProcessLost
        ),
        "this turn was abandoned after the process was lost while the conversation was being compacted. Compaction was not applied; the history before this point is unchanged. The compaction request may have been billed. No response from this turn is part of this history."
    );
    assert_eq!(
        abandoned_turn_note(
            AbandonedTurnClaim::CompactionNotApplied,
            AbandonmentCause::Interrupted
        ),
        "this turn stopped while the conversation was being compacted. Compaction was not applied; the history before this point is unchanged. The compaction request may have been billed. No response from this turn is part of this history."
    );
}
