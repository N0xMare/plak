//! Finalizing-commit retirement below the newest live round provider.

use super::{
    AgentError, AgentErrorKind, COMMAND_RETENTION_WINDOW, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, ModelOperationRoot, RuntimeLifecycle,
    RuntimeOperationLedger, RuntimeWorkingRoot, TurnId, composition_group, is_accepted_root,
    operation_turn_id, recorded_provider_response,
};
use crate::manifest::RetentionAnchor;
use crate::runtime::compaction::record::compaction_checkpoint_id;
use std::collections::{BTreeMap, BTreeSet};
mod dependencies;
mod selection;
use dependencies::{close_dependencies, record_dependencies};
#[cfg(test)]
pub(super) mod tests;

#[cfg(test)]
pub(in crate::runtime) fn retire_test_ledger(
    ledger: &RuntimeOperationLedger,
    turn: TurnId,
    high_water: usize,
) -> Result<RuntimeOperationLedger, AgentError> {
    retire_at(ledger, turn, &BTreeSet::new(), high_water)
}

const HIGH_WATER: usize = 2_048;

pub(in crate::runtime) fn has_retirement_boundary(
    prior: &RuntimeOperationLedger,
    staged: &RuntimeOperationLedger,
) -> bool {
    staged.operations().iter().any(|record| {
        // An in-cell process join is a nested settlement, not a round boundary.
        !matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
            && matches!(record.state(), DurableOperationState::Finalized(_))
            && prior
                .operation(record.operation_id())
                .is_none_or(|before| !matches!(before.state(), DurableOperationState::Finalized(_)))
    })
}

pub(in crate::runtime) fn retire_live_ledger(
    ledger: &RuntimeOperationLedger,
    turn: TurnId,
    owned: &BTreeSet<TurnId>,
) -> Result<RuntimeOperationLedger, AgentError> {
    let high_water = crate::test_hooks::ledger_retirement_high_water(HIGH_WATER);
    retire_at(ledger, turn, owned, high_water)
}

fn retire_at(
    ledger: &RuntimeOperationLedger,
    turn: TurnId,
    owned: &BTreeSet<TurnId>,
    high_water: usize,
) -> Result<RuntimeOperationLedger, AgentError> {
    let target = high_water * 3 / 4;
    let sweep = high_water / 4;
    let emergency = high_water * 3 / 2;
    if ledger.operation_count() < high_water {
        return Ok(ledger.clone());
    }
    ledger
        .validate_composition_evidence()
        .map_err(crate::runtime::continuation::runtime_manifest_error)?;
    let Some((_, mut pins)) = frontier(ledger, turn)? else {
        return Ok(ledger.clone());
    };
    for turn in owned {
        let root = ledger.operations().into_iter().find(|record| matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if bound == turn)).ok_or_else(corrupt)?;
        pins.insert(root.operation_id());
    }
    let removable = ledger
        .operations()
        .iter()
        .filter(|record| !pins.contains(&record.operation_id()))
        .count();
    if removable == 0 || (ledger.operation_count() < emergency && removable < sweep) {
        return Ok(ledger.clone());
    }
    let remove = selection::oldest_eligible(ledger, &pins, target, turn)?;
    if remove.is_empty() || ledger.operation_count() < emergency && remove.len() < sweep {
        return Ok(ledger.clone());
    }
    let commands = retained_command_sequences(ledger, turn, owned);
    let receipts = ledger
        .operations()
        .into_iter()
        .filter(|record| {
            !remove.contains(&record.operation_id())
                && matches!(record.kind(), DurableOperationKind::Command { .. })
        })
        .map(|record| record.request().bytes())
        .collect::<BTreeSet<_>>();
    let mut retained = ledger
        .retained(
            RetentionAnchor::ExistingHighest,
            |command| {
                commands.contains(&command.sequence())
                    || receipts.contains(command.command_component().bytes())
            },
            |record| !remove.contains(&record.operation_id()),
        )
        .map_err(crate::runtime::continuation::runtime_manifest_error)?;
    let attempts = removed_attempts(ledger, &retained, turn)?;
    let count = ledger
        .retired_attempts(turn)
        .checked_add(attempts)
        .ok_or_else(corrupt)?;
    retained = retained
        .with_retired_attempts((count != 0).then_some((turn, count)))
        .map_err(crate::runtime::continuation::runtime_manifest_error)?;
    validate_live_retirement(ledger, &retained, turn)?;
    Ok(retained.with_retirement_source(ledger, owned))
}

fn corrupt() -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        "operation ledger retirement violates its frontier or accounting",
    )
}

fn retained_command_sequences(
    ledger: &RuntimeOperationLedger,
    turn: TurnId,
    owned: &BTreeSet<TurnId>,
) -> BTreeSet<u64> {
    let commands = ledger.commands();
    let first = commands.len().saturating_sub(COMMAND_RETENTION_WINDOW);
    let latest = commands
        .iter()
        .rev()
        .find(|command| is_accepted_root(command))
        .map(|command| command.sequence());
    let unsettled = ledger
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(record.kind(), DurableOperationKind::Command { .. })
                && !matches!(record.state(), DurableOperationState::Finalized(_))
        })
        .map(|record| record.request().bytes())
        .collect::<BTreeSet<_>>();
    commands
        .iter()
        .enumerate()
        .filter(|(index, command)| {
            *index >= first
                || unsettled.contains(command.command_component().bytes())
                || is_accepted_root(command)
                    && (Some(command.sequence()) == latest
                        || command
                            .turn_id()
                            .is_some_and(|root| root == turn || owned.contains(&root)))
        })
        .map(|(_, command)| command.sequence())
        .collect()
}

fn removed_attempts(
    source: &RuntimeOperationLedger,
    retained: &RuntimeOperationLedger,
    turn: TurnId,
) -> Result<u64, AgentError> {
    source
        .operations()
        .iter()
        .filter(|record| {
            retained.operation(record.operation_id()).is_none()
                && operation_turn_id(record) == Some(turn)
                && matches!(
                    record.kind(),
                    DurableOperationKind::Effect { .. }
                        | DurableOperationKind::CompositionEvaluation { .. }
                        | DurableOperationKind::CompositionContinuation { .. }
                )
                && record.transitions().iter().any(|transition| {
                    matches!(transition.state(), DurableOperationState::Dispatched)
                })
        })
        .try_fold(0_u64, |sum, _| sum.checked_add(1).ok_or_else(corrupt))
}

pub(in crate::runtime) fn validate_live_retirement(
    source: &RuntimeOperationLedger,
    retained: &RuntimeOperationLedger,
    turn: TurnId,
) -> Result<(), AgentError> {
    let Some((p, pins)) = frontier(source, turn)? else {
        return if source == retained {
            Ok(())
        } else {
            Err(corrupt())
        };
    };
    if !retained.is_retained_successor_of(source) || !retained.preserves_high_water_of(source) {
        return Err(corrupt());
    }
    let mut dependencies = retained
        .operations()
        .iter()
        .map(|record| record.operation_id())
        .chain(pins.iter().copied())
        .collect::<BTreeSet<_>>();
    for record in retained.operations() {
        dependencies.extend(record_dependencies(record, turn)?);
    }
    close_dependencies(source, &mut dependencies, turn)?;
    if dependencies
        .iter()
        .any(|id| retained.operation(*id).is_none())
    {
        return Err(corrupt());
    }
    if !preserves_command_readers(source, retained, turn) {
        return Err(corrupt());
    }
    for record in source.operations() {
        if retained.operation(record.operation_id()).is_none()
            && (record.operation_id() >= p
                || pins.contains(&record.operation_id())
                || !matches!(record.state(), DurableOperationState::Finalized(_)))
        {
            return Err(corrupt());
        }
    }
    let attempts = source
        .retired_attempts(turn)
        .checked_add(removed_attempts(source, retained, turn)?)
        .ok_or_else(corrupt)?;
    if retained.retired_counter() != (attempts != 0).then_some((turn, attempts)) {
        return Err(corrupt());
    }
    Ok(())
}

fn preserves_command_readers(
    source: &RuntimeOperationLedger,
    retained: &RuntimeOperationLedger,
    turn: TurnId,
) -> bool {
    let required = retained_command_sequences(source, turn, &BTreeSet::new());
    let kept = retained
        .commands()
        .iter()
        .map(|command| command.sequence())
        .collect::<BTreeSet<_>>();
    let requests = retained
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Command { .. }))
        .map(|record| record.request().bytes())
        .collect::<BTreeSet<_>>();
    source
        .commands()
        .iter()
        .filter(|command| {
            required.contains(&command.sequence())
                || requests.contains(command.command_component().bytes())
        })
        .all(|command| kept.contains(&command.sequence()))
}

pub(in crate::runtime) fn ledger_matches_retirement(
    staged: &RuntimeOperationLedger,
    published: &RuntimeOperationLedger,
    lifecycle: &RuntimeLifecycle,
) -> bool {
    if staged == published {
        return true;
    }
    match lifecycle {
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } => validate_live_retirement(staged, published, *turn_id).is_ok(),
        _ => false,
    }
}

fn frontier(
    ledger: &RuntimeOperationLedger,
    turn: TurnId,
) -> Result<Option<(u64, BTreeSet<u64>)>, AgentError> {
    let records = ledger.operations();
    let Some(p) = records.iter().rev().find(|record| matches!(record.kind(),
        DurableOperationKind::Provider { root: ModelOperationRoot::Turn(root), .. } if *root == turn))
        .map(|record| record.operation_id()) else { return Ok(None); };
    if ledger
        .retired_counter()
        .is_some_and(|(root, _)| root != turn)
    {
        return Err(corrupt());
    }
    let mut pins = BTreeSet::new();
    let commands = ledger.commands();
    let kept_commands = retained_command_sequences(ledger, turn, &BTreeSet::new());
    let receipts = commands
        .iter()
        .filter(|command| kept_commands.contains(&command.sequence()))
        .map(|command| command.command_component().bytes())
        .collect::<BTreeSet<_>>();
    let root = records
        .iter()
        .find(|record| {
            matches!(record.kind(),
        DurableOperationKind::Command { turn_id: Some(root) } if *root == turn)
        })
        .map(|record| record.operation_id());
    for record in &records {
        if record.operation_id() >= p
            || !matches!(record.state(), DurableOperationState::Finalized(_))
            || matches!(record.kind(), DurableOperationKind::ProcessSession { .. })
            || matches!(record.kind(), DurableOperationKind::Command { .. })
                && (Some(record.operation_id()) == root
                    || receipts.contains(record.request().bytes()))
            || matches!(record.kind(), DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(root), .. } if *root == turn)
        {
            pins.insert(record.operation_id());
        }
    }
    provider_pins(ledger, turn, &mut pins)?;
    close_dependencies(ledger, &mut pins, turn)?;
    Ok(Some((p, pins)))
}

fn workflow(record: &DurableOperationRecord) -> Option<crate::SessionWorkflowId> {
    match record.kind() {
        DurableOperationKind::Compaction { workflow_id }
        | DurableOperationKind::Provider {
            root: ModelOperationRoot::SessionWorkflow(workflow_id),
            ..
        } => Some(*workflow_id),
        _ => None,
    }
}

fn provider_pins(
    ledger: &RuntimeOperationLedger,
    turn: TurnId,
    pins: &mut BTreeSet<u64>,
) -> Result<(), AgentError> {
    let records = ledger.operations();
    let root = records
        .iter()
        .find(|record| {
            matches!(record.kind(),
        DurableOperationKind::Command { turn_id: Some(root) } if *root == turn)
        })
        .map_or(0, |record| record.operation_id());
    let mut response = None;
    let mut finalized = None;
    let mut success = None;
    let mut workflows = BTreeSet::new();
    for record in &records {
        let DurableOperationKind::Provider {
            root: provider_root,
            ..
        } = record.kind()
        else {
            continue;
        };
        if matches!(
            record.state(),
            DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
        ) && recorded_provider_response(record)?
            .is_some_and(|response| !response.output().is_empty())
        {
            response = Some(record.operation_id());
        }
        if !matches!(record.state(), DurableOperationState::Finalized(_)) {
            continue;
        }
        finalized = Some(record.operation_id());
        if let ModelOperationRoot::SessionWorkflow(workflow) = provider_root {
            let checkpoint = record
                .transitions()
                .last()
                .ok_or_else(corrupt)?
                .checkpoint_id();
            if compaction_checkpoint_id(*workflow, b"success")? == checkpoint {
                success = Some(*workflow);
            } else if record.operation_id() > root
                && compaction_checkpoint_id(*workflow, b"failed")? == checkpoint
                && record
                    .transitions()
                    .iter()
                    .any(|t| matches!(t.state(), DurableOperationState::Dispatched))
                && !record
                    .transitions()
                    .iter()
                    .any(|t| matches!(t.state(), DurableOperationState::ProvenNotDispatched(_)))
            {
                workflows.insert(*workflow);
            }
        }
    }
    pins.extend(response);
    pins.extend(finalized);
    workflows.extend(success);
    if let Some((fold, provider, _)) =
        crate::runtime::compaction::instruction::latest_harness_fold(ledger)
    {
        pins.extend([fold.operation_id(), provider.operation_id()]);
    }
    for record in records {
        if matches!(record.kind(), DurableOperationKind::Compaction { workflow_id }
            | DurableOperationKind::Provider { root: ModelOperationRoot::SessionWorkflow(workflow_id), .. }
            if workflows.contains(workflow_id))
        {
            pins.insert(record.operation_id());
        }
    }
    Ok(())
}

fn groups(ledger: &RuntimeOperationLedger) -> BTreeMap<u64, u64> {
    let records = ledger.operations();
    let child_calls = records
        .iter()
        .filter_map(|record| match record.kind() {
            DurableOperationKind::CompositionEvaluation {
                attempt_id,
                call_id,
                ..
            } => Some(((*attempt_id, *call_id), record.operation_id())),
            DurableOperationKind::CompositionChild {
                attempt_id,
                call_id,
                evaluation_operation_id,
                ..
            }
            | DurableOperationKind::CompositionContinuation {
                attempt_id,
                call_id,
                evaluation_operation_id,
                ..
            } => Some(((*attempt_id, *call_id), *evaluation_operation_id)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    records
        .iter()
        .filter_map(|record| {
            let owner = composition_group(record).or_else(|| match record.kind() {
                DurableOperationKind::Effect {
                    attempt_id,
                    call_id,
                    ..
                }
                | DurableOperationKind::EffectDecision {
                    attempt_id,
                    call_id,
                    ..
                } => child_calls.get(&(*attempt_id, *call_id)).copied(),
                _ => None,
            })?;
            Some((record.operation_id(), owner))
        })
        .collect()
}
