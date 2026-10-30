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
