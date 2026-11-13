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
                if !allowed(
                    floor.owner.as_ref(),
                    source.as_ref(),
                    receipt.groups[index].transfers.get(&line.id),
                ) {
                    return Err(failure(format!("foreign witness for line {}", line.id)));
                }
                witnessed.insert(line.id.clone(), floor.owner);
            }
        }
    }
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
        receipt.groups[index]
            .transfers
            .insert(line, OwnerTransfer { from, to });
    }
    receipt.save(services.paths()?)?;
    crate::carry::upgrade::crash_cut("move-before-transfer");
    transfer_and_sweep(services, receipt, index, &grouped).await?;
    finish_ready(services, receipt, index, held).await
}

async fn transfer_and_sweep(
    services: &Services,
    receipt: &Receipt,
    index: usize,
    grouped: &BTreeMap<String, Vec<String>>,
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
            }
        }
        for status in client.lines().await.map_err(failure)? {
            if !lines.contains(&status.line) {
                continue;
            }
            let (_, jobs) =
                plakous::tools::runner::decode_jobs_snapshot(status.jobs).map_err(failure)?;
            if status.foreground
                || jobs.iter().any(|job| {
                    matches!(
                        job.status,
                        plakous::tools::JobStatus::Running
                            | plakous::tools::JobStatus::OutcomeUnknown
                    )
                })
            {
                return Err(failure("a dormant group has live or unknown runner work"));
            }
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
    Ok(())
}

fn allowed(
    witness: Option<&WireOwner>,
    source: Option<&WireOwner>,
    intended: Option<&OwnerTransfer>,
) -> bool {
    witness == source
        || intended.is_some_and(|intended| {
            witness == Some(&intended.to) || witness.is_some() && witness == intended.from.as_ref()
        })
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
