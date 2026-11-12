//! Bounded closed receipts and compact fences that never permit forgotten replay.

use sha2::{Digest as _, Sha256};

use super::{HashSet, NamespaceKey, OperatorRefusal, Table, TerminalKey, TerminalStatus};

#[derive(Default)]
pub(super) struct Generations {
    current: String,
    superseded: HashSet<[u8; 32]>,
    closed: HashSet<[u8; 32]>,
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(super) fn terminal_hash(key: &TerminalKey) -> [u8; 32] {
    hash(&serde_json::to_vec(key).unwrap_or_default())
}

fn namespace_hash(key: &NamespaceKey) -> [u8; 32] {
    hash(&serde_json::to_vec(&(&key.generation, &key.namespace.principal)).unwrap_or_default())
}

pub(super) fn admit_generation(
    table: &mut Table,
    key: &NamespaceKey,
) -> Result<(), OperatorRefusal> {
    let scope = (key.host.clone(), key.namespace.state_key.clone());
    if let Some(generations) = table.scopes.get(&scope)
        && (generations
            .superseded
            .contains(&hash(key.generation.as_bytes()))
            || generations.closed.contains(&namespace_hash(key)))
    {
        return Err(OperatorRefusal::NamespaceExpired);
    }
    if table
        .scopes
        .get(&scope)
        .is_some_and(|g| g.current == key.generation)
    {
        return Ok(());
    }
    let records: usize = table
        .scopes
        .values()
        .map(|g| g.superseded.len() + g.closed.len())
        .sum();
    if records >= 4096 || (!table.scopes.contains_key(&scope) && table.scopes.len() >= 128) {
        return Err(OperatorRefusal::Quota);
    }
    let generations = table.scopes.entry(scope).or_default();
    if generations.current != key.generation {
        if !generations.current.is_empty() {
            generations
                .superseded
                .insert(hash(generations.current.as_bytes()));
        }
        generations.current.clone_from(&key.generation);
    }
    Ok(())
}

pub(super) fn permit_retirement(
    table: &Table,
    key: &NamespaceKey,
    target: &str,
) -> Result<(), OperatorRefusal> {
    let generations = table
        .scopes
        .get(&(key.host.clone(), key.namespace.state_key.clone()))
        .ok_or(OperatorRefusal::ReconcileRequired)?;
    if generations.current != key.generation {
        return Err(OperatorRefusal::NamespaceExpired);
    }
    if !generations.superseded.contains(&hash(target.as_bytes())) {
        return Err(OperatorRefusal::TerminalLost);
    }
    Ok(())
}

pub(super) fn collect(table: &mut Table) {
    for ns in table.namespaces.values_mut() {
        let mut done: Vec<_> = ns
            .terminals
            .iter()
            .filter(|(key, t)| {
                (t.status == TerminalStatus::Exited
                    || (t.status == TerminalStatus::Lost && ns.loss_reported.contains(*key)))
                    && t.controller.is_none()
            })
            .map(|(key, _)| key.clone())
            .collect();
        done.sort_by(|a, b| a.terminal_id.as_str().cmp(b.terminal_id.as_str()));
        let remove = done
            .len()
            .saturating_sub(8)
            .min(4096_usize.saturating_sub(ns.evicted.len()));
        for key in done.into_iter().take(remove) {
            ns.evicted.insert(terminal_hash(&key));
            ns.terminals.remove(&key);
            ns.loss_reported.remove(&key);
        }
    }
    let records: usize = table
        .scopes
        .values()
        .map(|g| g.superseded.len() + g.closed.len())
        .sum();
    let mut room = 4096_usize.saturating_sub(records);
    let scopes = &mut table.scopes;
    table.namespaces.retain(|key, ns| {
        if room == 0
            || !ns.closed
            || ns.terminals.values().any(super::Terminal::pinned)
            || ns
                .terminals
                .iter()
                .any(|(key, t)| t.status == TerminalStatus::Lost && !ns.loss_reported.contains(key))
        {
            return true;
        }
        let generations = scopes
            .entry((key.host.clone(), key.namespace.state_key.clone()))
            .or_default();
        if generations.closed.insert(namespace_hash(key)) {
            room -= 1;
        }
        false
    });
    table.output_budgets.retain(|(host, principal), _| {
        table
            .namespaces
            .keys()
            .any(|key| &key.host == host && &key.namespace.principal == principal)
    });
}
