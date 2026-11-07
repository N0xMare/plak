use super::*;
use crate::runtime::control::{command_binding, command_canonical_bytes, durable_rejection};
use crate::{
    CheckpointId, CommandId, ControlError, ControlErrorKind, DurableOperationTransition,
    TurnCommand,
};

#[test]
fn retention_keeps_owned_roots_outside_the_settled_command_window()
-> Result<(), Box<dyn std::error::Error>> {
    let owned = TurnId::try_from(uuid::Uuid::from_u128(1))?;
    let latest = TurnId::try_from(uuid::Uuid::from_u128(2))?;
    let mut commands = Vec::new();
    for sequence in 1..=4_096 {
        let accepted = sequence <= 2;
        let command = TurnCommand::Start {
            command_id: CommandId::new(format!("retention:{sequence}"))?,
            input: if accepted { "root" } else { "" }.into(),
        };
        let envelope = DurableCommandEnvelope {
            version: 1,
            canonical_input: command_canonical_bytes(&command),
            effect: if accepted {
                DurableActorCommandEffect::Root
            } else {
                DurableActorCommandEffect::Rejected
            },
            rejection: (!accepted).then(|| {
                durable_rejection(&ControlError::new(
                    ControlErrorKind::InvalidInput,
                    "empty prompt",
                ))
            }),
        };
        commands.push(DurableCommandRecord::new(
            command.command_id().as_str(),
            sequence,
            match sequence {
                1 => Some(owned),
                2 => Some(latest),
                _ => None,
            },
            command_binding(&command)?,
            runtime_component(
                RuntimeComponentKind::Command,
                serde_json::to_vec(&envelope)?,
            )?,
            if accepted {
                DurableCommandDisposition::Accepted
            } else {
                DurableCommandDisposition::Rejected
            },
            (!accepted).then(|| "InvalidInput".into()),
        )?);
    }
    let ledger = RuntimeOperationLedger::new(commands, Vec::new())?;
    let retained = retain_quiescent_ledger(&ledger, &BTreeSet::from([owned]))?;
    assert_eq!(retained.command_count(), COMMAND_RETENTION_WINDOW + 2);
    assert!(
        retained
            .commands()
            .iter()
            .any(|command| command.turn_id() == Some(owned))
    );
    assert!(
        retained
            .commands()
            .iter()
            .any(|command| command.turn_id() == Some(latest))
    );
    let settled = retain_quiescent_ledger(&ledger, &BTreeSet::new())?;
    assert_eq!(settled.command_count(), COMMAND_RETENTION_WINDOW + 1);
    assert!(
        !settled
            .commands()
            .iter()
            .any(|command| command.turn_id() == Some(owned))
    );
    Ok(())
}

fn root_command(sequence: u64, turn_id: TurnId) -> TestResult<DurableCommandRecord> {
    let command = TurnCommand::Start {
        command_id: CommandId::new(format!("root:{sequence}"))?,
        input: "root".into(),
    };
    let envelope = DurableCommandEnvelope {
        version: 1,
        canonical_input: command_canonical_bytes(&command),
        effect: DurableActorCommandEffect::Root,
        rejection: None,
    };
    Ok(DurableCommandRecord::new(
        command.command_id().as_str(),
        sequence,
        Some(turn_id),
        command_binding(&command)?,
        runtime_component(
            RuntimeComponentKind::Command,
            serde_json::to_vec(&envelope)?,
        )?,
        DurableCommandDisposition::Accepted,
        None,
    )?)
}

fn rejected_command(sequence: u64) -> TestResult<DurableCommandRecord> {
    let command = TurnCommand::Start {
        command_id: CommandId::new(format!("rejected:{sequence}"))?,
        input: "".into(),
    };
    let envelope = DurableCommandEnvelope {
        version: 1,
        canonical_input: command_canonical_bytes(&command),
        effect: DurableActorCommandEffect::Rejected,
        rejection: Some(durable_rejection(&ControlError::new(
            ControlErrorKind::InvalidInput,
            "empty prompt",
        ))),
    };
    Ok(DurableCommandRecord::new(
        command.command_id().as_str(),
        sequence,
        None,
        command_binding(&command)?,
        runtime_component(
            RuntimeComponentKind::Command,
            serde_json::to_vec(&envelope)?,
        )?,
        DurableCommandDisposition::Rejected,
        Some("InvalidInput".into()),
    )?)
}

fn checkpoint(value: u128) -> TestResult<CheckpointId> {
    Ok(CheckpointId::try_from(uuid::Uuid::from_u128(value))?)
}

/// A finalized root operation of a turn that ran no provider call.
fn settled_root(operation_id: u64, turn_id: TurnId) -> TestResult<DurableOperationRecord> {
    Ok(DurableOperationRecord::new(
        operation_id,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        runtime_component(RuntimeComponentKind::Operation, b"root".to_vec())?,
        vec![DurableOperationTransition::new(
            checkpoint(u128::from(operation_id) << 8)?,
            DurableOperationState::Finalized(runtime_component(
                RuntimeComponentKind::Operation,
                b"terminal".to_vec(),
            )?),
        )],
    )?)
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn retired_attempt_counter_is_root_bound_authenticated_and_reconstructive() -> TestResult {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x29))?;
    let foreign = TurnId::try_from(uuid::Uuid::from_u128(0x2a))?;
    let ledger =
        RuntimeOperationLedger::new(vec![root_command(1, turn)?], vec![settled_root(1, turn)?])?;
    let source_identity = ledger.authority_identity()?;
    assert!(
        ledger
            .clone()
            .with_retired_attempts(Some((turn, 0)))
            .is_err()
    );
    assert!(
        ledger
            .clone()
            .with_retired_attempts(Some((foreign, 1)))
            .is_err()
    );
    let ledger = ledger.with_retired_attempts(Some((turn, 5_100)))?;
    assert_eq!(
        ledger.retired_attempts_bytes(),
        [
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x29, 0, 0, 0, 0, 0, 0, 0x13, 0xec,
        ]
    );
    assert_ne!(source_identity, ledger.authority_identity()?);
    assert_eq!(ledger.retired_attempts(turn), 5_100);
    assert_eq!(ledger.retired_attempts(foreign), 0);
    let ledger = retain_quiescent_ledger(&ledger, &BTreeSet::new())?;
    let source = crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        plakous_model::CanonicalHistory::default(),
        1,
        false,
        Some(ledger),
        None,
    )?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&source, None)?;
    let restored = crate::RuntimeCheckpointCodec::materialize(&bundle)?;
    assert_eq!(restored.manifest().ledger().retired_attempts(turn), 5_100);
    assert_eq!(restored.integrity(), source.integrity());
    assert_eq!(restored.manifest().ledger(), source.manifest().ledger());
    assert_eq!(
        restored
            .manifest()
            .ledger()
            .clone()
            .with_retired_attempts(None)?
            .retired_attempts(turn),
        0
    );
    Ok(())
}

#[test]
fn publication_rejects_unexplained_retired_counter_changes() -> TestResult {
    use crate::runtime::checkpoint::candidate::{SuccessorFacts, successor_candidate};
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x2b))?;
    let operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn),
        },
        runtime_component(RuntimeComponentKind::Operation, b"root".to_vec())?,
        vec![
            DurableOperationTransition::new(checkpoint(0x2c)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(checkpoint(0x2d)?, DurableOperationState::Dispatched),
        ],
    )?;
    let ledger = RuntimeOperationLedger::new(vec![root_command(1, turn)?], vec![operation])?
        .with_retired_attempts(Some((turn, 7)))?;
    let lifecycle = working_lifecycle(turn, &ledger)?;
    let source = crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        plakous_model::CanonicalHistory::default(),
        1,
        true,
        Some(ledger),
        Some(lifecycle),
    )?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&source, None)?;
    let manifest = source.manifest();
    for count in [None, Some(6), Some(7), Some(8)] {
        let ledger = manifest
            .ledger()
            .clone()
            .with_retired_attempts(count.map(|n| (turn, n)))?;
        let result = successor_candidate(
            &source,
            &bundle,
            SuccessorFacts {
                checkpoint_id: checkpoint(0x2e)?,
                lifecycle: manifest.lifecycle().clone(),
                history: manifest.canonical().history().clone(),
                ledger,
                jobs: manifest.jobs().clone(),
                named_state: manifest.named_state().clone(),
                context: manifest.canonical().context().retained_for_exact_resume(),
                family: manifest.family_artifact().cloned(),
            },
            1,
            None,
        );
        if count == Some(7) {
            assert!(result.is_ok());
        } else {
            assert_eq!(
                result.err().ok_or("counter change published")?.message(),
                "checkpoint successor changed retired attempts without retirement"
            );
        }
    }
    Ok(())
}

#[test]
fn retention_keeps_counter_root_beyond_the_command_window() -> TestResult {
    let prior = TurnId::try_from(uuid::Uuid::from_u128(0x2f))?;
    let latest = TurnId::try_from(uuid::Uuid::from_u128(0x30))?;
    let mut commands = vec![root_command(1, prior)?];
    for sequence in 2..300 {
        commands.push(rejected_command(sequence)?);
    }
    commands.push(root_command(300, latest)?);
    let ledger = RuntimeOperationLedger::new(
        commands,
        vec![settled_root(1, prior)?, settled_root(2, latest)?],
    )?
    .with_retired_attempts(Some((prior, 17)))?;
    let retained = retain_quiescent_ledger(&ledger, &BTreeSet::new())?;
    assert!(retained.operation(1).is_some());
    assert_eq!(retained.retired_attempts(prior), 17);
    Ok(())
}

#[test]
fn new_root_retention_drops_the_previous_newest_composition_group_whole() -> TestResult {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x30))?;
    let next = TurnId::try_from(uuid::Uuid::from_u128(0x31))?;
    let attempt = AttemptId::try_from(uuid::Uuid::from_u128(0x32))?;
    let call = crate::CallId::try_from(uuid::Uuid::from_u128(0x33))?;
    let evidence = runtime_component(RuntimeComponentKind::Operation, b"settled".to_vec())?;
    let finalized = |id, kind| -> TestResult<DurableOperationRecord> {
        DurableOperationRecord::new(
            id,
            kind,
            evidence.clone(),
            vec![
                DurableOperationTransition::new(checkpoint(0x34)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(
                    checkpoint(0x35)?,
                    DurableOperationState::Dispatched,
                ),
                DurableOperationTransition::new(
                    checkpoint(0x36)?,
                    DurableOperationState::Completed(evidence.clone()),
                ),
                DurableOperationTransition::new(
                    checkpoint(0x37)?,
                    DurableOperationState::Finalized(evidence.clone()),
                ),
            ],
        )
        .map_err(Into::into)
    };
    let ledger = RuntimeOperationLedger::new(
        vec![root_command(1, turn)?],
        vec![
            settled_root(1, turn)?,
            finalized(
                2,
                DurableOperationKind::CompositionEvaluation {
                    turn_id: turn,
                    attempt_id: attempt,
                    call_id: call,
                    source_index: 0,
                },
            )?,
            finalized(
                3,
                DurableOperationKind::CompositionChild {
                    turn_id: turn,
                    attempt_id: attempt,
                    call_id: call,
                    evaluation_operation_id: 2,
                    interpreter_generation: 1,
                    cell_id: "cell".into(),
                    request_sequence: 1,
                },
            )?,
        ],
    )?;
    assert_eq!(
        retain_quiescent_ledger(&ledger, &BTreeSet::new())?
            .operations()
            .len(),
        3
    );
    let ledger = ledger.append_command(root_command(2, next)?)?;
    // Admission supplies the actual pending record before capacity validation.
    let anchor = DurableOperationRecord::new(
        4,
        DurableOperationKind::Command {
            turn_id: Some(next),
        },
        evidence,
        vec![DurableOperationTransition::new(
            checkpoint(0x38)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let admitted = retain_quiescent_ledger_with_anchor(
        &ledger,
        &BTreeSet::new(),
        crate::manifest::RetentionAnchor::NewRecord(anchor),
    )?;
    assert_eq!(
        admitted
            .operations()
            .iter()
            .map(|record| record.operation_id())
            .collect::<Vec<_>>(),
        [1, 4]
    );
    assert_eq!(
        admitted.operations().last().ok_or("root")?.operation_id(),
        4
    );
    Ok(())
}

#[test]
fn retention_requires_an_actual_strictly_newer_sequence_anchor() -> TestResult {
    use crate::manifest::RetentionAnchor;
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x39))?;
    let ledger = RuntimeOperationLedger::new(
        vec![root_command(1, turn)?, rejected_command(2)?],
        vec![settled_root(1, turn)?, settled_root(2, turn)?],
    )?;
    let retained = ledger.retained(RetentionAnchor::ExistingHighest, |_| false, |_| false)?;
    assert_eq!(retained.operations()[0].operation_id(), 2);
    assert_eq!(retained.commands()[0].sequence(), 2);
    for id in [1, 2] {
        assert!(
            ledger
                .retained(
                    RetentionAnchor::NewRecord(settled_root(id, turn)?),
                    |_| false,
                    |_| false,
                )
                .is_err()
        );
    }
    let retained = ledger.retained(
        RetentionAnchor::NewRecord(settled_root(3, turn)?),
        |_| false,
        |_| false,
    )?;
    assert_eq!(retained.operations()[0].operation_id(), 3);
    assert!(retained.preserves_high_water_of(&ledger));
    Ok(())
}

#[test]
fn every_publication_refuses_cold_rebuilt_high_water_regression() -> TestResult {
    use crate::runtime::checkpoint::candidate::{SuccessorFacts, successor_candidate};
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x3a))?;
    let ledger = RuntimeOperationLedger::new(
        vec![root_command(1, turn)?, rejected_command(2)?],
        vec![settled_root(1, turn)?, settled_root(2, turn)?],
    )?;
    let source = crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        plakous_model::CanonicalHistory::default(),
        1,
        false,
        Some(ledger),
        None,
    )?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&source, None)?;
    let manifest = source.manifest();
    for (commands, operations) in [(1, 2), (2, 1), (2, 2)] {
        // Deliberately use the public cold constructor, not retention: the
        // publication boundary must catch regressions from every producer.
        let ledger = RuntimeOperationLedger::new(
            manifest.ledger().commands()[..commands]
                .iter()
                .copied()
                .cloned()
                .collect(),
            manifest.ledger().operations()[..operations]
                .iter()
                .copied()
                .cloned()
                .collect(),
        )?;
        let result = successor_candidate(
            &source,
            &bundle,
            SuccessorFacts {
                checkpoint_id: checkpoint(0x3b)?,
                lifecycle: RuntimeLifecycle::Quiescent,
                history: manifest.canonical().history().clone(),
                ledger,
                jobs: manifest.jobs().clone(),
                named_state: manifest.named_state().clone(),
                context: manifest.canonical().context().retained_for_exact_resume(),
                family: manifest.family_artifact().cloned(),
            },
            1,
            None,
        );
        if commands == 2 && operations == 2 {
            assert!(result.is_ok(), "equal high-water marks remain publishable");
        } else {
            let error = result.err().ok_or("regressed successor was accepted")?;
            assert_eq!(error.kind(), AgentErrorKind::Internal);
            assert_eq!(
                error.to_string(),
                "checkpoint successor regressed a ledger high-water mark"
            );
        }
    }
    Ok(())
}

#[test]
fn new_root_retention_never_drops_unresolved_operations() -> TestResult {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x40))?;
    let next = TurnId::try_from(uuid::Uuid::from_u128(0x41))?;
    let pending = DurableOperationRecord::new(
        2,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(turn),
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0x42))?,
        },
        runtime_component(RuntimeComponentKind::Operation, b"pending".to_vec())?,
        vec![DurableOperationTransition::new(
            checkpoint(0x43)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger = RuntimeOperationLedger::new(
        vec![root_command(1, turn)?, root_command(2, next)?],
        vec![settled_root(1, turn)?, pending.clone()],
    )?;
    let retained = retain_quiescent_ledger(&ledger, &BTreeSet::new())?;
    assert_eq!(retained.operations().last().copied(), Some(&pending));
    Ok(())
}

/// Turn A's fold failed after dispatch and the turn was refused; turn B,
/// the last root, was refused before any provider call. Retention of the
/// quiescent ledger past the high-water mark drops turn A's records, but
/// keeps the failed fold the paid-fold guard reads, so the host does not
/// pay for a second fold of the same source.
#[test]
fn retention_keeps_the_failed_fold_the_paid_fold_guard_reads() -> TestResult {
    let turn_a = TurnId::try_from(uuid::Uuid::from_u128(0xa))?;
    let turn_b = TurnId::try_from(uuid::Uuid::from_u128(0xb))?;
    let mut commands = vec![root_command(1, turn_a)?];
    for sequence in 2..=2_100 {
        commands.push(rejected_command(sequence)?);
    }
    commands.push(root_command(2_101, turn_b)?);
    let workflow = crate::SessionWorkflowId::new([0x5e; 32])?;
    let evidence = runtime_component(RuntimeComponentKind::Operation, b"failed".to_vec())?;
    let failed_fold = DurableOperationRecord::new(
        2,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::SessionWorkflow(workflow),
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xf0))?,
        },
        runtime_component(RuntimeComponentKind::Operation, b"fold".to_vec())?,
        vec![
            DurableOperationTransition::new(checkpoint(0x21)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(checkpoint(0x22)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                checkpoint(0x23)?,
                DurableOperationState::Completed(evidence.clone()),
            ),
            DurableOperationTransition::new(
                crate::runtime::compaction::record::compaction_checkpoint_id(workflow, b"failed")?,
                DurableOperationState::Finalized(evidence),
            ),
        ],
    )?;
    let ledger = RuntimeOperationLedger::new(
        commands,
        vec![
            settled_root(1, turn_a)?,
            failed_fold,
            settled_root(3, turn_b)?,
        ],
    )?;
    assert!(crate::runtime::driver::ledger_paid_fold_failed_since_last_round(&ledger));
    let retained = retain_quiescent_ledger(&ledger, &BTreeSet::new())?;
    let kept = retained
        .operations()
        .iter()
        .map(|record| record.operation_id())
        .collect::<Vec<_>>();
    assert_eq!(kept, vec![2, 3]);
    assert!(crate::runtime::driver::ledger_paid_fold_failed_since_last_round(&retained));
    Ok(())
}
