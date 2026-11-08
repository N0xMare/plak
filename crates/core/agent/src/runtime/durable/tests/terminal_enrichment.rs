use super::*;
use crate::runtime::terminal::{DurableTerminalResultRecord, encode_terminal_result_record};

#[tokio::test]
async fn historical_terminal_replay_refuses_non_enrichment_usage_changes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 88_400)?;
    model.unknown_outcome_from_call.store(0, Ordering::Release);
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let make = || session_update_builder(workspace.clone(), model.clone(), registry.clone());
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:historical-usage", make()?).await?;
    let command = CommandId::new("root:historical-usage")?;
    let original = agent
        .start(command.clone(), "unknown request")
        .await?
        .into_turn()
        .ok_or("original turn")?
        .result()
        .await?;
    assert!(matches!(
        original.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let checkpoint = original.checkpoint().clone();
    let _ = agent.shutdown().await;

    for tamper in [false, true] {
        let reader = historical_reader(
            &checkpoint,
            execution_value_fixture(registry.clone(), workspace.clone())?,
            &command,
            tamper,
        )?;
        let reply = reader
            .command(TurnCommand::Start {
                command_id: command.clone(),
                input: "unknown request".into(),
            })
            .await?;
        if tamper {
            let rejection = reply
                .acknowledgement()
                .rejection()
                .ok_or("reader accepted changed usage")?;
            assert_eq!(rejection.kind(), ControlErrorKind::Internal);
            assert_eq!(
                rejection.message(),
                "historical terminal checkpoint carries a different result record"
            );
            assert!(reply.result().is_none());
        } else {
            assert!(reply.acknowledgement().rejection().is_none());
            let result = reply
                .result()
                .ok_or("historical result")?
                .clone()
                .result()
                .await?;
            assert_eq!(result.usage(), original.usage());
            assert_eq!(result.checkpoint().id(), checkpoint.id());
        }
        reader.shutdown().await?;
    }
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "replay never dispatches"
    );
    Ok(())
}

fn historical_reader(
    checkpoint: &RuntimeCheckpoint,
    mut prepared: crate::runtime::PreparedAgent,
    command: &CommandId,
    tamper: bool,
) -> TestResult<Agent> {
    use crate::runtime::checkpoint::owner::PreparedCheckpoint;
    use crate::runtime::continuation::rehydrate_runtime_indexes;

    // Keep the historical checkpoint intact. Only the retained command
    // index differs, as when replay reads a later terminal record.
    let bundle = RuntimeCheckpointCodec::bundle(checkpoint, None)?;
    prepared.authority = PreparedCheckpoint::new(Arc::new(checkpoint.clone()), bundle, None);
    let (mut commands, next_command, next_operation) =
        rehydrate_runtime_indexes(checkpoint, &BTreeMap::new(), false)?;
    if tamper {
        commands
            .get_mut(command)
            .ok_or("retained command")?
            .terminal
            .as_mut()
            .ok_or("retained terminal")?
            .usage
            .record_model_operation(None)?;
    }
    prepared.initial_command_ledger = commands;
    prepared.next_command_sequence = next_command;
    prepared.next_operation_sequence = next_operation;
    Ok(prepared.spawn())
}

#[tokio::test]
async fn recovery_required_enrichment_replays_the_original_command() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 88_300)?;
    model.unknown_outcome_from_call.store(0, Ordering::Release);
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let make = || session_update_builder(workspace.clone(), model.clone(), registry.clone());
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:enrichment-replay", make()?).await?;
    let command = CommandId::new("root:enrichment-replay")?;
    let original = agent
        .start(command.clone(), "unknown request")
        .await?
        .into_turn()
        .ok_or("original turn")?
        .result()
        .await?;
    assert!(matches!(
        original.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let checkpoint = original.checkpoint().clone();
    let original_record = terminal_record_at_checkpoint(&checkpoint, &command)?;
    assert!(original_record.abandonment.is_none());
    let _ = agent.shutdown().await;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:enrichment-replay",
            DurableSessionAddress {
                session_id: checkpoint.manifest().topology().session_id,
                branch_id: checkpoint.manifest().topology().branch_id,
                checkpoint_id: checkpoint.id(),
            },
            make()?,
        )
        .await?
    else {
        return Err("exact reopen refused".into());
    };
    let report = reopened.abandoned_turn().ok_or("abandonment report")?;
    let mut enriched = terminal_record_at_checkpoint(&reopened.checkpoint(), &command)?;
    assert_eq!(enriched.abandonment.take().as_ref(), Some(report));
    assert_eq!(enriched, original_record);
    let duplicate = reopened
        .start(command.clone(), "unknown request")
        .await?
        .into_result()
        .ok_or("historical result")?
        .result()
        .await?;
    assert!(matches!(
        duplicate.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert_eq!(duplicate.checkpoint().id(), checkpoint.id());
    assert_eq!(duplicate.checkpoint().manifest(), checkpoint.manifest());
    assert_eq!(duplicate.usage(), original.usage());
    let refused = reopened
        .command(TurnCommand::Start {
            command_id: command,
            input: "different request".into(),
        })
        .await?;
    assert_eq!(
        refused
            .acknowledgement()
            .rejection()
            .ok_or("input conflict")?
            .kind(),
        ControlErrorKind::IdentityConflict
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "recovery and replay never dispatch"
    );
    reopened.shutdown().await?;
    Ok(())
}

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
    assert_enrichment_guards(&original, &enriched)?;
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

fn assert_enrichment_guards(
    original: &DurableTerminalResultRecord,
    enriched: &DurableTerminalResultRecord,
) -> TestResult {
    let accepts = |before: &DurableTerminalResultRecord,
                   after: &DurableTerminalResultRecord|
     -> TestResult<bool> {
        Ok(crate::runtime::valid_terminal_enrichment(
            original.turn_id,
            &encode_terminal_result_record(before)?,
            &encode_terminal_result_record(after)?,
        ))
    };
    assert!(accepts(original, enriched)?);
    assert!(!accepts(enriched, enriched)?, "no second enrichment");
    assert!(!accepts(original, original)?, "a report must be added");
    let mut cancelled = original.clone();
    cancelled.outcome = DurableTerminalOutcome::Cancelled(DurableAgentError::from_error(
        &AgentError::new(AgentErrorKind::Session, "cancelled"),
    )?);
    let mut cancelled_enriched = cancelled.clone();
    cancelled_enriched.abandonment = enriched.abandonment.clone();
    assert!(
        !accepts(&cancelled, &cancelled_enriched)?,
        "RecoveryRequired only"
    );
    let mut wrong_turn = original.clone();
    wrong_turn.turn_id = TurnId::try_from(uuid::Uuid::from_u128(88_106))?;
    let mut wrong_turn_enriched = wrong_turn.clone();
    wrong_turn_enriched.abandonment = enriched.abandonment.clone();
    assert!(!accepts(&wrong_turn, &wrong_turn_enriched)?, "turn binding");
    let mut changed = enriched.clone();
    changed.usage.record_model_operation(None)?;
    assert!(!accepts(original, &changed)?, "usage is immutable");
    changed = enriched.clone();
    changed.checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(88_103))?;
    assert!(!accepts(original, &changed)?, "checkpoint is immutable");
    changed = enriched.clone();
    changed.outcome = DurableTerminalOutcome::Cancelled(DurableAgentError::from_error(
        &AgentError::new(AgentErrorKind::Session, "cancelled"),
    )?);
    assert!(!accepts(original, &changed)?, "result is immutable");
    changed = enriched.clone();
    changed.conversation_identity = Digest::sha256(
        DigestDomain::CanonicalHistory,
        1,
        &[b"different conversation"],
    )?;
    assert!(!accepts(original, &changed)?, "conversation is immutable");
    changed = enriched.clone();
    changed.command_id.push_str(":different");
    assert!(!accepts(original, &changed)?, "command is immutable");
    changed = enriched.clone();
    changed.operation_id = OperationId(92);
    assert!(!accepts(original, &changed)?, "operation is immutable");
    changed = enriched.clone();
    changed.remedy = Some(crate::runtime::operation_record::DecidedRemedy::ClearOrFork);
    assert!(!accepts(original, &changed)?, "remedy is immutable");
    Ok(())
}
