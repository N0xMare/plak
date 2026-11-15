//! A foreign witness withholds the whole group; it never authorizes a higher fence.
use super::{Capsule, installation};
use crate::carry::upgrade::receipt::{GroupPhase, OwnerTransfer, Phase, Receipt};
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};
use plakous::runner::protocol::LineOwner as WireOwner;
use plakous::sessions::{AgentSessions, LeaseOnlyHead, SessionOperationId};
use std::collections::BTreeMap;

fn failure(error: impl std::fmt::Debug) -> CliFailure {
    CliFailure::new(format!("host_upgrade_group_withheld: {error:?}"))
}

/// Caller holds the same receipt lock as install and explicit disposition.
pub(crate) fn ready(services: &Services, receipt: &mut Receipt, index: usize) -> CliResult<()> {
    if crate::test_hooks::upgrade_move_failure("reconcile-pause") {
        return Err(failure("injected reconciliation hold"));
    }
    if receipt.phase != Phase::Committed
        || receipt.destination_key != crate::home::state::state_key()
        || receipt.destination_binary != crate::build_identity::binary_id()
        || !receipt.groups[index].source_retired
        || services
            .runner_ownership()
            .is_none_or(crate::commands::runner::ownership::RunnerOwnership::is_standalone)
        || receipt.groups[index].phase != GroupPhase::Published
    {
        return Err(failure("group is not awaiting reconciliation"));
    }
    let capsule = installation::capsule(services, receipt, index)?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    let executor = plakous::application::ApplicationExecutor::new().map_err(failure)?;
    let mut held = Vec::new();
    let result = executor
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(25),
                reconcile(services, receipt, index, &capsule, &sessions, &mut held),
            )
            .await
            .map_err(failure)?
        })
        .map_err(failure)
        .and_then(std::convert::identity);
    let mut release = Ok(());
    for head in held {
        let released = executor
            .block_on(head.release())
            .map_err(failure)
            .and_then(|result| result.map_err(failure));
        if release.is_ok() {
            release = released;
        }
    }
    result.and(release)
}

async fn reconcile(
    services: &Services,
    receipt: &mut Receipt,
    index: usize,
    capsule: &Capsule,
    sessions: &AgentSessions,
    held: &mut Vec<LeaseOnlyHead>,
) -> CliResult<()> {
    let grouped = lines_by_runner(capsule)?;
    let mut witnessed = BTreeMap::new();
    for (runner, lines) in &grouped {
        let client = if runner == "local" {
            services.local_runner_client()
        } else {
            services.runners().client(runner)
        }
        .ok_or_else(|| failure("runner is not Ready"))?;
        for batch in lines.chunks(256) {
            for floor in client.owner_floors(batch).await.map_err(failure)? {
                let line = capsule
                    .lines
                    .iter()
                    .find(|line| line.id == floor.line)
                    .ok_or_else(|| failure("unexpected runner line"))?;
                let source = line.source_owner.as_ref().map(source_owner);
                if floor.owner.is_none()
                    && floor.retained.is_empty()
                    && capsule.needs_runner(&line.id).map_err(failure)?
                {
                    return Err(failure(
                        "no original worker proves the carried nonterminal allocations",
                    ));
                }
                let intended = receipt.groups[index].transfers.get(&line.id);
                let from = witness_from(floor, source.as_ref(), intended, capsule.job_cut)?;
                witnessed.insert(line.id.clone(), from);
            }
        }
    }
    crate::carry::upgrade::crash_cut("move-after-owner-witnesses");
    for line in &capsule.lines {
        let operation =
            SessionOperationId::new(format!("plak:move-ready:{}:{}", capsule.group, line.id))
                .map_err(failure)?;
        let acquired = sessions
            .open_lease_only(&operation, line.id.parse().map_err(failure)?)
            .await
            .map_err(failure)?;
        if acquired.locator().session_id().to_string() != line.session
            || acquired.locator().branch_id().to_string() != line.branch
            || acquired.generation() != line.generation
        {
            held.push(acquired);
            return Err(failure("installed line no longer matches its capsule"));
        }
        held.push(acquired);
    }
    crate::carry::upgrade::crash_cut("move-after-lease-only-heads");
    for head in held.iter() {
        let line = head.line().to_string();
        let to = WireOwner {
            line: line.clone(),
            generation: head.generation(),
            branch: head.locator().branch_id().to_string(),
            fence: head.fence(),
        };
        let from = witnessed
            .get(&line)
            .ok_or_else(|| failure("missing fresh witness"))?
            .clone();
        let mut previous = receipt.groups[index]
            .transfers
            .get(&line)
            .map_or_else(Vec::new, |intent| intent.previous.clone());
        if let Some(intent) = receipt.groups[index].transfers.get(&line)
            && intent.to != to
            && !previous.contains(&intent.to)
        {
            previous.push(intent.to.clone());
        }
        if previous.len() > 64 {
            return Err(failure("own owner-intent history exceeds its bound"));
        }
        receipt.groups[index]
            .transfers
            .insert(line, OwnerTransfer { from, to, previous });
    }
    receipt.save(services.paths()?)?;
    crate::carry::upgrade::crash_cut("move-before-transfer");
    transfer_and_sweep(services, receipt, index, &grouped, capsule, held).await?;
    finish_ready(services, receipt, index, held).await
}

async fn transfer_and_sweep(
    services: &Services,
    receipt: &Receipt,
    index: usize,
    grouped: &BTreeMap<String, Vec<String>>,
    capsule: &Capsule,
    heads: &[LeaseOnlyHead],
) -> CliResult<()> {
    for (runner, lines) in grouped {
        let client = if runner == "local" {
            services.local_runner_client()
        } else {
            services.runners().client(runner)
        }
        .ok_or_else(|| failure("runner disconnected"))?;
        for line in lines {
            let transfer = &receipt.groups[index].transfers[line];
            let from = transfer.from.as_ref().map(tool_owner).transpose()?;
            let to = tool_owner(&transfer.to)?;
            let result = client
                .transfer_owner_exact(from.as_ref(), &to)
                .await
                .map_err(failure)?;
            let result = if crate::test_hooks::upgrade_move_failure("transfer-witness") {
                plakous::runner::ExactOwnerTransfer::WitnessMismatch(transfer.to.clone())
            } else {
                result
            };
            match result {
                plakous::runner::ExactOwnerTransfer::Accepted(stopped) if stopped.is_empty() => {}
                _ => {
                    return Err(failure(format!(
                        "exact transfer refused or found live foreground work on {line}"
                    )));
                }
            }
            crate::carry::upgrade::crash_cut("move-after-transfer");
        }
    }
    crate::carry::upgrade::crash_cut("move-after-all-transfers");
    // No runner's final witness is sampled until every exact transfer joined.
    for (runner, lines) in grouped {
        let client = if runner == "local" {
            services.local_runner_client()
        } else {
            services.runners().client(runner)
        }
        .ok_or_else(|| failure("runner disconnected before final owner sweep"))?;
        // The fresh sweep is observational: it must not run the ordinary auto-raising path.
        for batch in lines.chunks(256) {
            for mut floor in client.owner_floors(batch).await.map_err(failure)? {
                if crate::test_hooks::upgrade_move_failure("post-transfer-witness") {
                    floor.owner = None;
                }
                if floor.owner.as_ref() != Some(&receipt.groups[index].transfers[&floor.line].to) {
                    return Err(failure(format!(
                        "owner changed before Ready on {}",
                        floor.line
                    )));
                }
                if floor
                    .retained
                    .iter()
                    .any(|owner| owner != &receipt.groups[index].transfers[&floor.line].to)
                {
                    return Err(failure("an original worker changed owner before Ready"));
                }
            }
        }
        let statuses = client.lines().await.map_err(failure)?;
        let inventory = capsule
            .verify_runner_work(lines, &statuses)
            .map_err(failure)?;
        for head in heads
            .iter()
            .filter(|head| lines.contains(&head.line().to_string()))
        {
            crate::carry::upgrade::crash_cut("move-before-job-terminals");
            head.record_job_terminals(&inventory[&head.line().to_string()])
                .await
                .map_err(failure)?;
            crate::carry::upgrade::crash_cut("move-after-job-terminals");
        }
    }
    Ok(())
}

async fn finish_ready(
    services: &Services,
    receipt: &mut Receipt,
    index: usize,
    guards: &mut [LeaseOnlyHead],
) -> CliResult<()> {
    for head in guards {
        head.renew().await.map_err(failure)?;
    }
    crate::carry::upgrade::crash_cut("move-before-ready");
    receipt.groups[index].phase = GroupPhase::Ready;
    let products = &receipt.groups[index].products;
    for mapping in &mut receipt.mappings {
        if products.contains(&mapping.source) {
            mapping.ready = true;
        }
    }
    if receipt
        .mappings
        .iter()
        .all(|mapping| mapping.ready || mapping.abandoned)
    {
        receipt.phase = Phase::Acknowledged;
    }
    receipt.save(services.paths()?)?;
    crate::carry::upgrade::crash_cut("move-after-ready");
    if let Err(error) = release_disposed(services, receipt).await {
        eprintln!("host upgrade witness release deferred; group remains Ready: {error:?}");
    }
    Ok(())
}

/// Disposition stays durable while the native client retries harmless cleanup.
pub(crate) async fn release_disposed(services: &Services, receipt: &mut Receipt) -> CliResult<()> {
    let mut failures = Vec::new();
    for group in receipt
        .groups
        .iter()
        .filter(|group| matches!(group.phase, GroupPhase::Ready | GroupPhase::Abandoned))
    {
        for runner in &group.runners {
            let Some(client) = (if runner == "local" {
                services.local_runner_client()
            } else {
                services.runners().client(runner)
            }) else {
                let detail = format!(
                    "group {} runner {runner}: disconnected before witness release",
                    group.id
                );
                eprintln!("host upgrade witness release deferred: {detail}");
                failures.push(detail);
                continue;
            };
            let lines = group.lines.iter().cloned().collect::<Vec<_>>();
            for batch in lines.chunks(256) {
                if let Err(error) = client.forget_owner_witnesses(batch).await {
                    let detail = format!("group {} runner {runner}: {error:?}", group.id);
                    eprintln!("host upgrade witness release deferred: {detail}");
                    failures.push(detail);
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failure(failures))
    }
}

fn allowed(
    witness: Option<&WireOwner>,
    source: Option<&WireOwner>,
    intended: Option<&OwnerTransfer>,
) -> bool {
    witness == source
        || intended.is_some_and(|intended| {
            witness == Some(&intended.to)
                || witness.is_some() && witness == intended.from.as_ref()
                || witness.is_some_and(|witness| intended.previous.contains(witness))
        })
}

fn witness_from(
    floor: plakous::runner::protocol::OwnerFloor,
    source: Option<&WireOwner>,
    intended: Option<&OwnerTransfer>,
    cut: super::JobCut,
) -> CliResult<Option<WireOwner>> {
    let native_allowed = allowed(floor.owner.as_ref(), source, intended)
        || floor.owner.is_none() && matches!(cut, super::JobCut::Retained);
    if !native_allowed
        || floor
            .retained
            .iter()
            .any(|owner| !allowed(Some(owner), source, intended))
    {
        return Err(failure(format!("foreign witness for line {}", floor.line)));
    }
    // A new native peer has no floor yet. Its CAS must still name the
    // imported source or our saved intent at every original.
    Ok(floor
        .owner
        .or_else(|| intended.map(|intent| intent.to.clone()))
        .or_else(|| source.cloned()))
}

fn tool_owner(owner: &WireOwner) -> CliResult<plakous::tools::LineOwner> {
    Ok(plakous::tools::LineOwner::new(
        owner.line.parse().map_err(failure)?,
        owner.generation,
        owner.branch.parse().map_err(failure)?,
        owner.fence,
    ))
}

fn source_owner(owner: &super::RunnerOwner) -> WireOwner {
    WireOwner {
        line: owner.line.clone(),
        generation: owner.generation,
        branch: owner.branch.clone(),
        fence: owner.fence,
    }
}

fn lines_by_runner(capsule: &Capsule) -> CliResult<BTreeMap<String, Vec<String>>> {
    let mut grouped = BTreeMap::<_, Vec<_>>::new();
    for line in &capsule.lines {
        let context = capsule.current_context(line).map_err(failure)?;
        grouped
            .entry(context.runner.clone())
            .or_default()
            .push(line.id.clone());
    }
    Ok(grouped)
}

#[cfg(test)]
mod tests;
