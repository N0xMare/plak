//! Only a durable never-Ready intent may withdraw an entire installed group.
use crate::carry::upgrade::receipt::{GroupPhase, Phase, Receipt};
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};
use plakous::sessions::host_move::{MoveDisposition, MoveOperation};
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, BranchHold, SessionOperationId};

fn failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(format!("host_upgrade_group_abandon: {error}"))
}

pub(crate) fn select(receipt: &Receipt, target: &str) -> CliResult<Option<usize>> {
    if let Some(index) = receipt.groups.iter().position(|group| group.id == target) {
        return Ok(Some(index));
    }
    let Some(index) = receipt
        .groups
        .iter()
        .position(|group| group.trees.contains(target) || group.products.contains(target))
    else {
        return Ok(None);
    };
    if receipt.groups[index].trees.len() != 1 {
        return Err(failure(format!(
            "name explicit connected group {} to abandon all linked trees",
            receipt.groups[index].id
        )));
    }
    Ok(Some(index))
}

/// The host holds the receipt publication lock across this entire operation.
pub(crate) fn abandon(services: &Services, receipt: &mut Receipt, index: usize) -> CliResult<()> {
    let own = crate::home::state::state_key();
    if !matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
        || receipt.destination_key != own
        || receipt.destination_binary != crate::build_identity::binary_id()
        || receipt.groups[index].phase == GroupPhase::Ready
        || services
            .runner_ownership()
            .is_none_or(crate::commands::runner::ownership::RunnerOwnership::is_standalone)
    {
        return Err(failure("Ready or foreign groups cannot be abandoned"));
    }
    if receipt.groups[index].phase == GroupPhase::Abandoned {
        let executor = plakous::application::ApplicationExecutor::new().map_err(failure)?;
        if let Err(error) = executor
            .block_on(super::reconciliation::release_disposed(services, receipt))
            .map_err(failure)?
        {
            eprintln!("host upgrade disposed-group witness release deferred: {error:?}");
        }
        return Ok(());
    }
    receipt.groups[index].phase = GroupPhase::Abandoning;
    receipt.save(services.paths()?)?;
    crate::carry::upgrade::crash_cut("move-before-abandon");
    let operation = MoveOperation::new(receipt.groups[index].operation).map_err(failure)?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    let executor = plakous::application::ApplicationExecutor::new().map_err(failure)?;
    let mut guards = Vec::new();
    let result = executor
        .block_on(dispose_native(
            &sessions,
            receipt,
            index,
            operation,
            &mut guards,
        ))
        .map_err(failure)
        .and_then(std::convert::identity);
    for guard in guards {
        let _ = executor.block_on(guard.release());
    }
    if let Err(error) = result {
        receipt.groups[index].abandon_error = Some(error.to_string().chars().take(512).collect());
        receipt.groups[index].abandon_retry_after = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_add(180),
        );
        receipt.save(services.paths()?)?;
        return Err(error);
    }
    crate::carry::upgrade::crash_cut("move-after-native-abandon");
    receipt.groups[index].phase = GroupPhase::Abandoned;
    receipt.groups[index].abandon_error = None;
    receipt.groups[index].abandon_retry_after = None;
    let products = &receipt.groups[index].products;
    for mapping in &mut receipt.mappings {
        if products.contains(&mapping.source) {
            mapping.abandoned = true;
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
    if let Err(error) = executor
        .block_on(super::reconciliation::release_disposed(services, receipt))
        .map_err(failure)?
    {
        eprintln!("host upgrade disposed-group witness release deferred: {error:?}");
    }
    Ok(())
}

async fn dispose_native(
    sessions: &AgentSessions,
    receipt: &Receipt,
    index: usize,
    operation: MoveOperation,
    guards: &mut Vec<BranchHold>,
) -> CliResult<()> {
    let Some(digest) = receipt.groups[index].digest()? else {
        return match sessions.recover_move(operation).await {
            Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => Ok(()),
            _ => Err(failure(
                "native state exists without a recorded target stage",
            )),
        };
    };
    // Unguarded disposal is Staged-only (or an already completed Abandon).
    if sessions
        .finish_move(operation, digest, MoveDisposition::Abandon)
        .await
        .is_ok()
    {
        return Ok(());
    }
    let native = match sessions.recover_move(operation).await {
        Ok(native) => native,
        Err(error) => {
            // A crash after the guarded native commit but before the receipt write.
            return sessions
                .abandon_unopened_move(operation, digest, &[])
                .await
                .map_err(|_| failure(error));
        }
    };
    if native.digest() != digest {
        return Err(failure("native stage digest changed"));
    }
    for line in native.lines() {
        let current = line.line.head();
        let checkpoint = native
            .sessions()
            .iter()
            .flat_map(|session| &session.branches)
            .find(|branch| branch.session == current)
            .ok_or_else(|| failure("missing staged line head"))?
            .checkpoint;
        let id = SessionOperationId::new(format!(
            "plak:move-abandon:{}:{}",
            receipt.groups[index].id,
            line.line.line()
        ))
        .map_err(failure)?;
        guards.push(
            sessions
                .hold_branch(
                    &id,
                    plakous::sessions::SessionLocator::new(
                        current.session_id(),
                        current.branch_id(),
                        checkpoint,
                    ),
                )
                .await
                .map_err(failure)?,
        );
    }
    sessions
        .abandon_unopened_move(operation, digest, &guards.iter().collect::<Vec<_>>())
        .await
        .map_err(failure)
}
