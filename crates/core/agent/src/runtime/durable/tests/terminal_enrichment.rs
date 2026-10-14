use super::*;
use crate::runtime::terminal::{DurableTerminalResultRecord, encode_terminal_result_record};

#[tokio::test]
async fn a_terminal_result_reads_the_recorded_remedy_not_the_conversation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            88_200,
            false,
        )
        .with_provider_overflows([0]),
    );
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:recorded-remedy",
            durable_artifact_test_builder(workspace, model),
        )
        .await?;
    let result = agent.prompt("withheld prompt").await?.result().await?;
    assert_eq!(
        result.context_overflow_remedy(),
        Some(crate::ContextOverflowRemedy::ResendShorter)
    );
    let mut terminal = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find_map(|record| match record.state() {
            DurableOperationState::Finalized(evidence) => {
                decode_terminal_result_record(evidence.bytes())
            }
            _ => None,
        })
        .ok_or("terminal")?;
    terminal.remedy = Some(crate::runtime::operation_record::DecidedRemedy::ClearOrFork);
    let recovered = crate::runtime::terminal::terminal_turn_result(
        &terminal,
        Arc::new(result.checkpoint().clone()),
    )?;
    assert_eq!(
        recovered.context_overflow_remedy(),
        Some(crate::ContextOverflowRemedy::ClearOrFork)
    );
    agent.shutdown().await?;
    Ok(())
}
use crate::runtime::{AbandonedTurnClaim, AbandonedTurnReport, AbandonmentCause};

#[test]
fn recovery_required_terminal_enrichment_is_add_only_and_once() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(88_101))?;
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(88_102))?;
    let next_checkpoint = CheckpointId::try_from(uuid::Uuid::from_u128(88_103))?;
    let evidence = runtime_component(
        RuntimeComponentKind::Operation,
        encode_pending_terminal_record(PendingTerminalResultRecord {
            remedy: None,
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: "enrichment".to_owned(),
            turn_id,
            operation_id: OperationId(91),
            outcome: DurableTerminalOutcome::RecoveryRequired(DurableAgentError::from_error(
                &AgentError::new(AgentErrorKind::OutcomeUnknown, "uncertain"),
            )?),
            usage: TurnUsage::default(),
        })?,
    )?;
    let finalized = finalized_operation_evidence(
        &evidence,
        checkpoint_id,
        Digest::sha256(DigestDomain::CanonicalHistory, 1, &[b"enrichment"])?,
    )?;
    let original = decode_terminal_result_record(finalized.bytes()).ok_or("terminal")?;
    let record = DurableOperationRecord::new(
        91,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        evidence,
        vec![DurableOperationTransition::new(
            checkpoint_id,
            DurableOperationState::Finalized(finalized),
        )],
    )?;
    let mut enriched = original.clone();
    enriched.abandonment = Some(AbandonedTurnReport::new(
        vec![92],
        vec![],
        vec![],
        AbandonedTurnClaim::ModelRequest,
        AbandonmentCause::Interrupted,
    ));
    let transition =
        |terminal: &DurableTerminalResultRecord| -> TestResult<DurableOperationTransition> {
            Ok(DurableOperationTransition::new(
                next_checkpoint,
                DurableOperationState::Finalized(runtime_component(
                    RuntimeComponentKind::Operation,
                    encode_terminal_result_record(terminal)?,
                )?),
            ))
        };
    let updated = record.append_transition(transition(&enriched)?)?;
    assert_eq!(updated.transitions()[0], record.transitions()[0]);
    assert!(
        updated
            .append_transition(DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(88_104))?,
                updated.state().clone()
            ))
            .is_err()
    );
    assert!(record.append_transition(transition(&original)?).is_err());
    let mut changed = enriched.clone();
    changed.usage.record_model_operation(None)?;
    assert!(record.append_transition(transition(&changed)?).is_err());
    changed = enriched.clone();
    changed.checkpoint_id = next_checkpoint;
    assert!(record.append_transition(transition(&changed)?).is_err());
    changed = enriched.clone();
    changed.outcome = DurableTerminalOutcome::Cancelled(DurableAgentError::from_error(
        &AgentError::new(AgentErrorKind::Session, "cancelled"),
    )?);
    assert!(record.append_transition(transition(&changed)?).is_err());
    for kind in [
        DurableOperationKind::Command { turn_id: None },
        DurableOperationKind::Command {
            turn_id: Some(TurnId::try_from(uuid::Uuid::from_u128(88_105))?),
        },
    ] {
        assert!(
            DurableOperationRecord::new(
                91,
                kind,
                record.request().clone(),
                updated.transitions().to_vec()
            )
            .is_err()
        );
    }
    Ok(())
}
