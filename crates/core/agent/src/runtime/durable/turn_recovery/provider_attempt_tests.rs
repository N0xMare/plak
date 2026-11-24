use super::super::super::tests::landed_round_abandonment::{ExtraCall, cut_text_turn};
use super::*;
use plakous_model::{ModelError, ModelErrorKind, Usage};

#[tokio::test]
async fn recovery_terminal_counts_failed_and_retried_attempts_once() -> TestResult {
    let cut = cut_text_turn(ExtraCall::None).await?;
    let original = records(&cut);
    let provider = original
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider")?;
    let turn_id = operation_turn_id(provider).ok_or("turn")?;
    let DurableOperationState::Completed(evidence) = provider.state() else {
        return Err("completed".into());
    };
    let (tag, response) =
        crate::runtime::operation_record::test_support::recorded_response_parts(evidence)?;
    let reported = Usage::new(17, 5, 0, 0, 0);
    let response = ModelResponse::new(
        response.output().to_vec(),
        response.finish().clone(),
        Some(reported),
        response.response_id().map(str::to_owned),
        response.plan_digest(),
    );
    let response_evidence = runtime_component(
        RuntimeComponentKind::Operation,
        crate::runtime::operation_record::test_support::recorded_response_bytes(tag, response)?,
    )?;
    // The earlier attempt hit the output limit and the driver decided its
    // retry; the retry is the newest round and landed the answer.
    {
        for control_transition_count in 0..=4 {
            let failure = crate::runtime::operation_record::test_support::outcome_component(Err(
                &AgentError::from_model(ModelError::new(
                    ModelErrorKind::OutputLimit,
                    "earlier attempt failed",
                )),
            ))?;
            let failure = runtime_component(
                RuntimeComponentKind::Operation,
                crate::runtime::operation_record::test_support::recorded_decision_bytes(
                    &failure,
                    RoundDecision::RetryOutputLimit,
                )?,
            )?;
            let failed = DurableOperationRecord::new(
                provider.operation_id(),
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::Turn(turn_id),
                    attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_971))?,
                },
                provider.request().clone(),
                vec![
                    DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_972))?,
                        DurableOperationState::Prepared,
                    ),
                    DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_973))?,
                        DurableOperationState::Dispatched,
                    ),
                    DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_974))?,
                        DurableOperationState::Completed(failure.clone()),
                    ),
                    DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_975))?,
                        DurableOperationState::Finalized(failure),
                    ),
                ],
            )?;
            let mut transitions = provider.transitions().to_vec();
            let last = transitions.last_mut().ok_or("last")?;
            *last = DurableOperationTransition::new(
                last.checkpoint_id(),
                DurableOperationState::Completed(response_evidence.clone()),
            );
            let newest = DurableOperationRecord::new(
                provider.operation_id() + 1,
                provider.kind().clone(),
                provider.request().clone(),
                transitions,
            )?;
            let mut changed = original
                .iter()
                .filter(|record| record.operation_id() != provider.operation_id())
                .cloned()
                .collect::<Vec<_>>();
            changed.extend([failed, newest]);
            let changed = checkpoint_with_review_state(
                &cut,
                changed,
                cut.manifest().canonical().history().clone(),
                false,
            )?;
            let changed = if control_transition_count == 0 {
                changed
            } else {
                controls::tests::with_control(
                    &changed,
                    crate::runtime::DurableActorCommandEffect::SteerQueued { position: 1 },
                    control_transition_count,
                )?
            };
            let fold = local_terminal_recovery_fold(&changed)?;
            let (_, evidence) = fold.synthesized.as_ref().ok_or("terminal")?;
            let terminal = decode_pending_terminal_record(evidence.bytes()).ok_or("decode")?;
            assert!(matches!(
                terminal.outcome,
                DurableTerminalOutcome::Completed(_)
            ));
            assert_eq!(terminal.usage.model_operations(), 2);
            assert_eq!(terminal.usage.model_operations_reporting_usage(), 1);
            assert_eq!(terminal.usage.model().input_tokens(), 17);
            assert_eq!(terminal.usage.model().output_tokens(), 5);
            let closed = terminal_closure_state(
                &changed,
                fold,
                false,
                CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_976))?,
            )?;
            let root = closed
                .ledger
                .operations()
                .into_iter()
                .find(|record| matches!(record.kind(), DurableOperationKind::Command { .. }))
                .ok_or("closed root")?;
            let DurableOperationState::Finalized(evidence) = root.state() else {
                return Err("root not finalized".into());
            };
            let finalized =
                decode_terminal_result_record(evidence.bytes()).ok_or("closed terminal")?;
            assert_eq!(finalized.usage, terminal.usage);
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_failed_attempt_requires_canonical_error_evidence() -> TestResult {
    let cut = cut_text_turn(ExtraCall::None).await?;
    let original = records(&cut);
    let provider = original
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider")?;
    let failure = crate::runtime::operation_record::test_support::outcome_component(Err(
        &AgentError::from_model(ModelError::new(ModelErrorKind::Protocol, "failed attempt")),
    ))?;
    for bytes in [b"[\"provider-outcome-v2\",{}]".to_vec(), {
        let mut bytes = failure.bytes().to_vec();
        bytes.push(b' ');
        bytes
    }] {
        let evidence = runtime_component(RuntimeComponentKind::Operation, bytes)?;
        let mut transitions = provider.transitions().to_vec();
        let last = transitions.last_mut().ok_or("last")?;
        *last = DurableOperationTransition::new(
            last.checkpoint_id(),
            DurableOperationState::Completed(evidence.clone()),
        );
        transitions.push(DurableOperationTransition::new(
            CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_978))?,
            DurableOperationState::Finalized(evidence),
        ));
        let failed = DurableOperationRecord::new(
            provider.operation_id(),
            provider.kind().clone(),
            provider.request().clone(),
            transitions,
        )?;
        let error = provider_outcome(&failed)
            .err()
            .ok_or("malformed failure accepted")?;
        assert_ne!(error.kind(), AgentErrorKind::OutcomeUnknown);
    }
    Ok(())
}

#[tokio::test]
async fn prepared_never_dispatched_provider_keeps_live_and_recovered_model_operations_zero()
-> TestResult {
    let cut = cut_text_turn(ExtraCall::None).await?;
    let records = records(&cut)
        .into_iter()
        .map(|record| {
            if matches!(record.kind(), DurableOperationKind::Provider { .. }) {
                DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    record.transitions()[..1].to_vec(),
                )
            } else {
                Ok(record)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cut = checkpoint_with_review_state(
        &cut,
        records,
        cut.manifest().canonical().history().clone(),
        false,
    )?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    let turn = fold.turn_id;
    let closed = abandonment_closure_state(
        &cut,
        fold,
        AbandonedTurnClaim::NothingStarted,
        CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_981))?,
    )?;
    let terminal = closed
        .ledger
        .operations()
        .into_iter()
        .find_map(|record| {
            if let DurableOperationState::Finalized(evidence) = record.state() {
                decode_terminal_result_record(evidence.bytes())
            } else {
                None
            }
        })
        .ok_or("terminal")?;
    assert_eq!(terminal.turn_id, turn);
    assert_eq!(
        terminal.usage.model_operations(),
        TurnUsage::default().model_operations()
    );
    assert_eq!(terminal.usage.model_operations(), 0);
    Ok(())
}
