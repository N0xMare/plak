//! Delete closed referrers before the evidence they still reference.

use super::{
    AgentError, BTreeMap, BTreeSet, RuntimeOperationLedger, TurnId, corrupt, groups,
    record_dependencies, workflow,
};

#[derive(Default)]
struct Unit {
    records: BTreeSet<u64>,
    references: BTreeSet<u64>,
    incoming: usize,
    pinned: bool,
}

pub(super) fn oldest_eligible(
    ledger: &RuntimeOperationLedger,
    pins: &BTreeSet<u64>,
    target: usize,
    turn: TurnId,
) -> Result<BTreeSet<u64>, AgentError> {
    let membership = membership(ledger);
    let mut units = units(ledger, &membership, pins, turn)?;
    let mut ready = units
        .iter()
        .filter(|(_, unit)| !unit.pinned && unit.incoming == 0)
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let mut removed = BTreeSet::new();
    while ledger.operation_count() - removed.len() > target {
        let Some(id) = ready.pop_first() else {
            break;
        };
        let unit = units.remove(&id).ok_or_else(corrupt)?;
        removed.extend(unit.records);
        for dependency in unit.references {
            let referenced = units.get_mut(&dependency).ok_or_else(corrupt)?;
            referenced.incoming = referenced.incoming.checked_sub(1).ok_or_else(corrupt)?;
            if !referenced.pinned && referenced.incoming == 0 {
                ready.insert(dependency);
            }
        }
    }
    Ok(removed)
}

fn membership(ledger: &RuntimeOperationLedger) -> BTreeMap<u64, u64> {
    let records = ledger.operations();
    let mut membership = groups(ledger);
    let mut workflows = BTreeMap::new();
    for record in &records {
        if let Some(workflow) = workflow(record) {
            let unit = workflows.entry(workflow).or_insert(record.operation_id());
            membership.insert(record.operation_id(), *unit);
        }
    }
    let mut oldest = BTreeMap::<u64, u64>::new();
    for record in records {
        let unit = *membership
            .entry(record.operation_id())
            .or_insert(record.operation_id());
        oldest
            .entry(unit)
            .and_modify(|first| *first = (*first).min(record.operation_id()))
            .or_insert(record.operation_id());
    }
    for unit in membership.values_mut() {
        *unit = oldest[unit];
    }
    membership
}

fn units(
    ledger: &RuntimeOperationLedger,
    membership: &BTreeMap<u64, u64>,
    pins: &BTreeSet<u64>,
    turn: TurnId,
) -> Result<BTreeMap<u64, Unit>, AgentError> {
    let mut units = BTreeMap::<u64, Unit>::new();
    for record in ledger.operations() {
        let id = membership[&record.operation_id()];
        let unit = units.entry(id).or_default();
        unit.records.insert(record.operation_id());
        unit.pinned |= pins.contains(&record.operation_id());
        // Parse even obsolete frames: deletion cannot hide malformed evidence.
        for dependency in record_dependencies(record, turn)? {
            let referenced = *membership.get(&dependency).ok_or_else(corrupt)?;
            if referenced != id {
                unit.references.insert(referenced);
            }
        }
    }
    let references = units
        .values()
        .flat_map(|unit| unit.references.iter().copied())
        .collect::<Vec<_>>();
    for referenced in references {
        units.get_mut(&referenced).ok_or_else(corrupt)?.incoming += 1;
    }
    Ok(units)
}
