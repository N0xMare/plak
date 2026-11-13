//! Passive installation runs only inside destination host custody and receipt locking.
use super::{Capsule, admission};
use crate::carry::upgrade::receipt::{GroupPhase, Phase, Receipt};
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};
use plakous::sessions::host_move::{MoveDisposition, MoveInstall, MoveOperation};
use plakous::sessions::{AgentSessionsErrorKind, SessionLocator};
use plakous_recursive_subagents::tree::{TreeStore as _, host_move::DormantTree};
use std::collections::BTreeMap;

fn failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(format!("host_upgrade_group_install: {error}"))
}

/// Caller holds the publication lock. No execution driver is constructed here.
pub(crate) fn install(services: &Services, receipt: &mut Receipt, index: usize) -> CliResult<()> {
    if receipt.phase != Phase::Committed
        || receipt.destination_key != crate::home::state::state_key()
        || receipt.destination_binary != crate::build_identity::binary_id()
        || services
            .runner_ownership()
            .is_none_or(crate::commands::runner::ownership::RunnerOwnership::is_standalone)
    {
        return Err(failure(
            "destination identity or serving custody does not match",
        ));
    }
    let group = receipt
        .groups
        .get(index)
        .ok_or_else(|| failure("missing group"))?;
    if !group.source_retired {
        return Err(failure(
            "the retained producer has not acknowledged permanent native retirement",
        ));
    }
    if matches!(
        group.phase,
        GroupPhase::Ready | GroupPhase::Abandoning | GroupPhase::Abandoned
    ) {
        return Err(failure("group is already disposed"));
    }
    let source_capsule = capsule(services, receipt, index)?;
    let operation = MoveOperation::new(group.operation).map_err(failure)?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    let executor = plakous::application::ApplicationExecutor::new().map_err(failure)?;
    let recovered = executor
        .block_on(sessions.recover_move(operation))
        .map_err(failure)?;
    let native = match recovered {
        Ok(native) => {
            if group.target_digest.as_deref()
                != Some(plakous::model::hex_lower(native.digest().bytes()).as_str())
            {
                return Err(failure("native stage conflicts with its recorded digest"));
            }
            native
        }
        Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => {
            if group.phase != GroupPhase::Pending {
                return Err(failure("a journaled native stage is missing"));
            }
            let (_, native) =
                admission::compile(services, source_capsule, admission::CompileMode::Host)?;
            receipt.groups[index].target_digest =
                Some(plakous::model::hex_lower(native.digest().bytes()));
            receipt.save(services.paths()?)?;
            crate::carry::upgrade::crash_cut("move-before-stage");
            executor
                .block_on(sessions.stage_move(operation, native.clone()))
                .map_err(failure)?
                .map_err(failure)?;
            native
        }
        Err(error) => return Err(failure(error)),
    };
    crate::carry::upgrade::crash_cut("move-after-stage");
    if receipt.groups[index].phase == GroupPhase::Pending {
        receipt.groups[index].phase = GroupPhase::Staged;
    }
    receipt.save(services.paths()?)?;
    // Recovering a stage never recompiles its grants or changes its checkpoints.
    let capsule = capsule(services, receipt, index)?;
    seed_semantics(services, &capsule, &native, receipt)?;
    executor
        .block_on(sessions.finish_move(operation, native.digest(), MoveDisposition::Publish))
        .map_err(failure)?
        .map_err(failure)?;
    crate::carry::upgrade::crash_cut("move-after-publish");
    receipt.groups[index].phase = GroupPhase::Published;
    receipt.save(services.paths()?)
}

pub(crate) fn capsule(services: &Services, receipt: &Receipt, index: usize) -> CliResult<Capsule> {
    let group = receipt
        .groups
        .get(index)
        .ok_or_else(|| failure("missing group"))?;
    let path = receipt
        .path(services.paths()?)
        .with_file_name(format!("{}.json", group.id));
    let (capsule, digest) =
        admission::read_with_digest(&path)?.ok_or_else(|| failure("wrong document kind"))?;
    if capsule.operation != group.operation
        || capsule.group != group.id
        || capsule.source_key != receipt.source_key
        || capsule.source_binary != receipt.source_binary
        || capsule.source_instance != receipt.source_instance
        || capsule.source_host != crate::commands::runner::existing_host_id(services.paths()?)?
        || capsule
            .products
            .iter()
            .map(|product| product.id().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
            != group.products
        || capsule
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            != group.sessions
        || capsule
            .lines
            .iter()
            .map(|line| line.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            != group.lines
        || capsule
            .sessions
            .iter()
            .flat_map(|session| &session.heads)
            .map(|head| head.context.runner.clone())
            .collect::<std::collections::BTreeSet<_>>()
            != group.runners
        || receipt
            .mappings
            .iter()
            .filter(|mapping| group.products.contains(&mapping.source))
            .any(|mapping| mapping.digest != digest)
    {
        return Err(failure(
            "capsule disagrees with its reserved identities or digest",
        ));
    }
    Ok(capsule)
}

fn seed_semantics(
    services: &Services,
    capsule: &Capsule,
    native: &MoveInstall,
    receipt: &Receipt,
) -> CliResult<()> {
    let operation = plakous::model::hex_lower(&capsule.operation);
    let group = receipt
        .groups
        .iter()
        .find(|group| group.id == capsule.group)
        .ok_or_else(|| failure("missing group"))?;
    let digest = &receipt
        .mappings
        .iter()
        .find(|mapping| group.products.contains(&mapping.source))
        .ok_or_else(|| failure("group has no reserved products"))?
        .digest;
    let heads = reconstructed_heads(capsule, native)?;
    let mut product = crate::store::ProductStore::open(services.paths()?).map_err(failure)?;
    for document in &capsule.products {
        let source = document.source().to_string();
        let target = heads
            .get(&source)
            .ok_or_else(|| failure("product head was not staged"))?;
        let seed = document
            .clone()
            .into_cut()
            .map_err(failure)?
            .reconstruct(target.parse().map_err(failure)?)
            .map_err(failure)?;
        product
            .install_move_seed(&operation, digest, &seed)
            .map_err(failure)?;
        crate::carry::upgrade::crash_cut("move-after-product-seed");
    }
    if !capsule.trees.is_empty() {
        let tree_store = plakous_recursive_subagents_store::SqliteTreeStore::open(
            &services.paths()?.subagents_database(),
        )
        .map_err(failure)?;
        for tree in &capsule.trees {
            let rows = tree.cut.clone().reconstruct(&heads).map_err(failure)?;
            let cut = DormantTree::capture(rows.rows()).map_err(failure)?;
            tree_store
                .install_move_seed(&operation, digest, &cut, &tree.custody.decode()?)
                .map_err(|error| failure(error.detail))?;
            crate::carry::upgrade::crash_cut("move-after-tree-seed");
        }
    }
    Ok(())
}

pub(super) fn reconstructed_heads(
    capsule: &Capsule,
    native: &MoveInstall,
) -> CliResult<BTreeMap<String, String>> {
    let mut heads = BTreeMap::new();
    for source in capsule.sessions.iter().flat_map(|session| &session.heads) {
        let locator: SessionLocator = source.source.parse().map_err(failure)?;
        let target = native
            .sessions()
            .iter()
            .flat_map(|session| &session.branches)
            .find(|branch| {
                branch.session.session_id() == locator.session_id()
                    && branch.session.branch_id() == locator.branch_id()
            })
            .ok_or_else(|| failure("source branch has no staged checkpoint"))?;
        heads.insert(
            source.source.clone(),
            SessionLocator::new(locator.session_id(), locator.branch_id(), target.checkpoint)
                .to_string(),
        );
    }
    Ok(heads)
}
