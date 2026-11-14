//! The receipt decision owns every connected group's irreversible source surrender.
use super::{
    CliFailure, CliResult, Leases, Phase, Ready, Receipt, Request, Services, check_cancel,
    new_receipt, preparation, process, receipt,
};
use crate::carry::host_move::{Capsule, GroupProbe, source::Inventory};
use plakous::sessions::host_move::MoveOperation;
use plakous::sessions::{BranchHold, SessionLocator, SessionOperationId};
use plakous_hosted::host_service::wire::SessionHost;
use std::sync::{Arc, atomic::AtomicBool};

mod source;

pub(super) struct Frozen {
    services: Services,
    id: uuid::Uuid,
    holds: Vec<BranchHold>,
    proofs: Vec<crate::carry::host_move::source::Proof>,
    trees: Vec<(
        Arc<plakous_recursive_subagents_store::SqliteTreeStore>,
        plakous_recursive_subagents::tree::CustodyHold,
    )>,
    committing: bool,
    decision_pending: bool,
}

impl Frozen {
    pub(super) fn decision_pending(&self) -> bool {
        self.decision_pending
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        let result = (|| {
            let paths = self.services.paths()?;
            let mut receipt = receipt::read(
                &receipt::root(paths)
                    .join(self.id.to_string())
                    .join("receipt.json"),
            )?;
            if !self.committing && matches!(receipt.phase, Phase::Prepared | Phase::Aborted) {
                preparation::withdraw(paths, &mut receipt)?;
            }
            Ok::<_, CliFailure>(())
        })();
        if let Err(error) = result {
            eprintln!(
                "ownership-move source remains fenced pending receipt recovery: {}",
                error.message()
            );
        }
        if let Ok(executor) = plakous::application::ApplicationExecutor::new() {
            for hold in self.holds.drain(..) {
                let _ = executor.block_on(hold.release());
            }
        }
        for (store, hold) in self.trees.drain(..) {
            let _ = plakous_recursive_subagents::tree::release_custody_hold(store.as_ref(), hold);
        }
    }
}

pub(super) fn prepare(
    host: &Arc<SessionHost<crate::surface_protocol::PlakExtension>>,
    services: &Services,
    request: Request,
    cancelled: &Arc<AtomicBool>,
    arguments: &[String],
    runtime: &tokio::runtime::Handle,
    progress: &(dyn Fn(&str) -> CliResult<()> + Sync),
) -> CliResult<(Ready, Option<uuid::Uuid>)> {
    let paths = services.paths()?;
    let operation =
        plakous::model::sha256_bytes(&[b"host-move/request/v1", uuid::Uuid::new_v4().as_bytes()]);
    let source::Prepared {
        connections: _bound_connections,
        inventory: final_inventory,
        pause,
        standalone,
        drain,
    } = source::prepare(
        host, services, &request, cancelled, runtime, progress, operation,
    )?;
    let mut authority = new_receipt(services, request, arguments)?;
    authority.save(paths)?;
    let mut ready = Ready {
        move_guard: Some(Frozen {
            services: services.clone(),
            id: authority.id,
            holds: Vec::new(),
            proofs: Vec::new(),
            trees: Vec::new(),
            committing: false,
            decision_pending: false,
        }),
        step_drain: drain,
        executable: authority.destination.clone(),
        key: authority.destination_key.clone(),
        binary: authority.destination_binary.clone(),
        leases: Leases::new()?,
        pause,
        _standalone: standalone,
    };
    crate::carry::upgrade::crash_cut("move-after-receipt");
    if let Some(inventory) = final_inventory {
        hold_trees(
            services,
            &inventory,
            ready
                .move_guard
                .as_mut()
                .ok_or_else(|| CliFailure::new("source guard missing"))?,
        )?;
        let guarded = Inventory::read_cut(services, operation, inventory.job_cut())?
            .ok_or_else(|| CliFailure::new("source inventory disappeared under its guards"))?;
        if guarded.groups != inventory.groups {
            return Err(CliFailure::new(
                "source membership changed while acquiring tree custody; no source surrendered",
            ));
        }
        for index in 0..inventory.groups.len() {
            prepare_group(
                services,
                &guarded,
                index,
                runtime,
                &mut authority,
                &mut ready,
            )?;
            check_cancel(cancelled)?;
        }
    }
    reserve_journal_capacity(&authority)?;
    authority.source_archival_pending = true;
    authority.save(paths)?;
    preparation::archive_sources(paths, &authority, true)?;
    crate::carry::upgrade::crash_cut("move-after-source-archive");
    check_cancel(cancelled)?;
    process::verify_image(&authority.destination, &authority.destination_binary)?;
    commit_source(services, &mut authority, &mut ready, runtime)?;
    Ok((ready, Some(authority.id)))
}

fn commit_source(
    services: &Services,
    authority: &mut Receipt,
    ready: &mut Ready,
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    let paths = services.paths()?;
    let guard = ready
        .move_guard
        .as_mut()
        .ok_or_else(|| CliFailure::new("source move guard missing"))?;
    for proof in &guard.proofs {
        proof.recheck(services, runtime)?;
    }
    ready.pause.commit()?;
    guard.committing = true;
    authority.phase = Phase::Committed;
    authority.source_archival_pending = false;
    if let Err(failure) = save_decision(paths, authority) {
        // Publication may already have happened. Keep the committed pause and
        // stop this owner; startup resolves the durable receipt before any open.
        eprintln!(
            "host upgrade decision requires startup reconciliation: {}",
            failure.message()
        );
        guard.decision_pending = true;
        return Ok(());
    }
    crate::carry::upgrade::crash_cut("move-after-commit");
    let retirement = if crate::test_hooks::upgrade_move_failure("source-finish") {
        Err(CliFailure::new("injected source retirement failure"))
    } else {
        crate::carry::host_move::source::finish_source(services, authority.id)
    };
    if let Err(failure) = retirement {
        eprintln!(
            "host upgrade source retirement deferred to destination recovery: {}",
            failure.message()
        );
    }
    Ok(())
}

fn save_decision(paths: &crate::home::paths::PlakPaths, receipt: &Receipt) -> CliResult<()> {
    if crate::test_hooks::upgrade_move_failure("commit-before-save") {
        return Err(CliFailure::new(
            "injected uncertain decision before publication",
        ));
    }
    receipt.save(paths)?;
    if crate::test_hooks::upgrade_move_failure("commit-after-save") {
        return Err(CliFailure::new(
            "injected uncertain decision after publication",
        ));
    }
    Ok(())
}

fn verify_reservation(reserved: &receipt::Group, capsule: &Capsule) -> CliResult<()> {
    let sessions = capsule
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect();
    let lines = capsule.lines.iter().map(|line| line.id.clone()).collect();
    let runners = capsule
        .sessions
        .iter()
        .flat_map(|session| &session.heads)
        .map(|head| head.context.runner.clone())
        .collect();
    if reserved.sessions != sessions || reserved.lines != lines || reserved.runners != runners {
        return Err(CliFailure::new(
            "host_upgrade_inventory_changed: prepared group differs from its durable reservation; no source surrendered",
        ));
    }
    Ok(())
}

fn reserve_journal_capacity(authority: &Receipt) -> CliResult<()> {
    let bytes =
        serde_json::to_vec(authority).map_err(|error| CliFailure::new(error.to_string()))?;
    // Each bounded UUID owner tuple fits well within 1 KiB, including both CAS
    // witnesses. Reserve space for all intents and remaining group dispositions.
    let growth = authority
        .groups
        .iter()
        .map(|group| group.lines.len() * 1024 + 4096)
        .sum::<usize>();
    if bytes.len().saturating_add(growth) as u64 > receipt::MAX_BYTES {
        return Err(CliFailure::new(
            "host_upgrade_group_capacity: readiness journal would exceed its bound; no source surrendered",
        ));
    }
    Ok(())
}

fn retain_connections(
    services: &Services,
    inventory: &Inventory,
    runtime: &tokio::runtime::Handle,
) -> CliResult<Vec<plakous::runner::RunnerConnectionHold>> {
    let runners = inventory
        .runner_ids(services.paths()?)?
        .into_iter()
        .filter(|runner| runner != "local")
        .collect::<Vec<_>>();
    retain_runners(services, runners, runtime)
}

fn retain_runners(
    services: &Services,
    runners: Vec<String>,
    runtime: &tokio::runtime::Handle,
) -> CliResult<Vec<plakous::runner::RunnerConnectionHold>> {
    let holds = runners
        .iter()
        .map(|id| {
            services.runners().retain_connection(id).ok_or_else(|| {
                CliFailure::new("host_upgrade_move_blocked: source runner is no longer configured")
            })
        })
        .collect::<CliResult<Vec<_>>>()?;
    runtime
        .block_on(services.runners().refresh(Some(&runners)))
        .map_err(|error| {
            CliFailure::new(format!(
                "host_upgrade_move_blocked: source runner refresh failed: {error:?}"
            ))
        })?;
    if runners.iter().any(|id| {
        services
            .runners()
            .client(id)
            .is_none_or(|client| !client.is_connected())
    }) {
        return Err(CliFailure::new(
            "host_upgrade_move_blocked: source runner connection is unavailable",
        ));
    }
    Ok(holds)
}

fn preflight(
    services: &Services,
    request: &Request,
    inventory: &Inventory,
    cancelled: &AtomicBool,
    runtime: &tokio::runtime::Handle,
    progress: &(dyn Fn(&str) -> CliResult<()> + Sync),
) -> CliResult<()> {
    let directory = tempfile::tempdir_in(services.paths()?.home()).map_err(super::io_failure)?;
    for index in 0..inventory.groups.len() {
        check_cancel(cancelled)?;
        let snapshot = inventory.snapshot(services, index, runtime)?;
        let capsule = inventory.capsule(services, index, &snapshot.native, runtime)?;
        let trees = inventory.groups[index].trees().iter().collect::<Vec<_>>();
        for members in trees.chunks(32) {
            progress(&format!(
                "preflight connected {} ({} trees, {} native sessions): {}",
                capsule.group,
                trees.len(),
                inventory.groups[index].sessions().len(),
                members
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))?;
        }
        if index == 0 && crate::test_hooks::upgrade_move_failure("probe-idle-window") {
            std::thread::sleep(std::time::Duration::from_secs(6));
        }
        probe(services, request, &capsule, directory.path())?;
    }
    Ok(())
}

fn hold_trees(services: &Services, inventory: &Inventory, frozen: &mut Frozen) -> CliResult<()> {
    if inventory.persisted_trees().next().is_none() {
        return Ok(());
    }
    let store =
        Arc::new(crate::store::tree::open_tree_store(services.paths()?).map_err(CliFailure::new)?);
    for tree in inventory.persisted_trees() {
        let hold = plakous_recursive_subagents::tree::acquire_custody(store.as_ref(), tree)
            .map_err(|error| CliFailure::new(error.to_string()))?;
        frozen.trees.push((Arc::clone(&store), hold));
    }
    Ok(())
}

fn prepare_group(
    services: &Services,
    inventory: &Inventory,
    index: usize,
    runtime: &tokio::runtime::Handle,
    receipt: &mut Receipt,
    ready: &mut Ready,
) -> CliResult<()> {
    let paths = services.paths()?;
    let initial = inventory.snapshot(services, index, runtime)?;
    let capsule = inventory.capsule(services, index, &initial.native, runtime)?;
    reserve(receipt, &capsule)?;
    receipt.save(paths)?;
    crate::carry::upgrade::crash_cut("move-after-cleanup-intent");
    let frozen = ready
        .move_guard
        .as_mut()
        .ok_or_else(|| CliFailure::new("source guard missing"))?;
    let start = frozen.holds.len();
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    freeze_branches(services, &initial, &capsule.group, frozen, runtime)?;
    let operation = MoveOperation::new(capsule.operation)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    for proof in &initial.proofs {
        proof.recheck(services, runtime)?;
    }
    frozen.proofs.extend(initial.proofs);
    let native = executor
        .block_on(sessions.prepare_move_cut(
            operation,
            &frozen.holds[start..].iter().collect::<Vec<_>>(),
            initial.native.job_cut(),
        ))
        .map_err(|error| CliFailure::new(error.to_string()))?
        .map_err(|error| CliFailure::new(error.to_string()))?;
    crate::carry::upgrade::crash_cut("move-after-source-prepare");
    let mut capsule = inventory.capsule(services, index, &native, runtime)?;
    if crate::test_hooks::upgrade_move_failure("reservation-lines") {
        let _ = capsule.lines.pop();
    }
    if crate::test_hooks::upgrade_move_failure("reservation-runners")
        && let Some(head) = capsule
            .sessions
            .iter_mut()
            .flat_map(|session| &mut session.heads)
            .next()
    {
        head.context.runner = "unreserved-runner".into();
    }
    verify_reservation(&receipt.groups[index], &capsule)?;
    let directory = receipt.path(paths).with_file_name("");
    let bytes = serde_json::to_vec(&capsule).map_err(|error| CliFailure::new(error.to_string()))?;
    write_capsule(&directory.join(format!("{}.json", capsule.group)), &bytes)?;
    capsule.write_backups(paths, &directory)?;
    let digest = plakous::model::sha256_hex(&[&bytes]);
    for mapping in &mut receipt.mappings {
        if receipt.groups[index].products.contains(&mapping.source) {
            mapping.digest.clone_from(&digest);
        }
    }
    receipt.groups[index].source_digest = Some(plakous::model::hex_lower(native.digest().bytes()));
    receipt.save(paths)?;
    let request = Request {
        source_instance: receipt.source_instance.clone(),
        source_key: receipt.source_key.clone(),
        destination: receipt.destination.clone(),
        destination_key: receipt.destination_key.clone(),
        expected_binary: receipt.destination_binary.clone(),
        wait_seconds: 0,
        drain: crate::command::HostDrain::default(),
        abandon: None,
    };
    probe(services, &request, &capsule, &directory)?;
    crate::carry::upgrade::crash_cut("move-after-group-export");
    Ok(())
}

fn freeze_branches(
    services: &Services,
    initial: &crate::carry::host_move::source::Snapshot,
    group: &str,
    frozen: &mut Frozen,
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    for proof in &initial.proofs {
        proof.recheck(services, runtime)?;
    }
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    for session in initial.native.sessions() {
        for branch in &session.branches {
            let locator = SessionLocator::new(
                branch.session.session_id(),
                branch.session.branch_id(),
                branch.checkpoint,
            );
            let operation = SessionOperationId::new(format!(
                "plak:move-freeze:{group}:{}",
                branch.session.branch_id()
            ))
            .map_err(|error| CliFailure::new(error.to_string()))?;
            let hold = executor
                .block_on(sessions.hold_branch(&operation, locator))
                .map_err(|error| CliFailure::new(error.to_string()))?
                .map_err(|error| CliFailure::new(error.to_string()))?;
            frozen.holds.push(hold);
            if let Some(line) = initial
                .native
                .lines()
                .iter()
                .find(|line| line.line.head() == branch.session)
            {
                let hold = frozen
                    .holds
                    .last()
                    .ok_or_else(|| CliFailure::new("source hold missing"))?;
                executor
                    .block_on(sessions.record_move_terminals(hold, line, branch.checkpoint))
                    .map_err(|error| CliFailure::new(error.to_string()))?
                    .map_err(|error| CliFailure::new(error.to_string()))?;
            }
        }
    }
    Ok(())
}

fn reserve(receipt: &mut Receipt, capsule: &Capsule) -> CliResult<()> {
    let mut trees = std::collections::BTreeSet::new();
    let mut products = std::collections::BTreeSet::new();
    let provisional = plakous::model::sha256_hex(&[
        &serde_json::to_vec(capsule).map_err(|error| CliFailure::new(error.to_string()))?
    ]);
    for product in &capsule.products {
        let (record, _, _) = product
            .clone()
            .into_cut()
            .map_err(CliFailure::new)?
            .into_parts();
        trees.insert(record.node.tree_id.clone());
        products.insert(record.id.clone());
        receipt.mappings.push(receipt::Mapping {
            ready: false,
            abandoned: false,
            runner: record
                .runner
                .as_ref()
                .map_or("local", |runner| runner.runner_id.as_str())
                .into(),
            source: record.id.clone(),
            successor: record.id,
            source_node: plakous_host_protocol::wire::NodeAddress {
                tree_id: record.node.tree_id,
                agent_id: record.node.agent_id,
            },
            digest: provisional.clone(),
        });
    }
    receipt.groups.push(receipt::Group {
        id: capsule.group.clone(),
        operation: capsule.operation,
        products,
        trees,
        sessions: capsule
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect(),
        lines: capsule.lines.iter().map(|line| line.id.clone()).collect(),
        runners: capsule
            .sessions
            .iter()
            .flat_map(|session| &session.heads)
            .map(|head| head.context.runner.clone())
            .collect(),
        source_digest: None,
        source_retired: false,
        target_digest: None,
        phase: receipt::GroupPhase::Pending,
        transfers: std::collections::BTreeMap::new(),
        abandon_error: None,
        abandon_retry_after: None,
    });
    receipt.validate()
}

fn probe(
    services: &Services,
    request: &Request,
    capsule: &Capsule,
    directory: &std::path::Path,
) -> CliResult<()> {
    let path = directory.join(format!("{}.json", capsule.group));
    write_capsule(
        &path,
        &serde_json::to_vec(capsule).map_err(|error| CliFailure::new(error.to_string()))?,
    )?;
    let mut command = process::command(
        &request.destination,
        services.paths()?.home(),
        &request.destination_key,
    );
    command
        .args(["version", "--host-upgrade-capacity"])
        .arg(path);
    let reply: GroupProbe = serde_json::from_slice(
        &process::run(command, super::PROBE_BOUND)?.stdout,
    )
    .map_err(|_| CliFailure::new("destination did not answer its connected-group install probe"))?;
    if reply.install_protocol != Some(1) || reply.group != capsule.group {
        return Err(CliFailure::new(
            "host_upgrade_move_unsupported: destination cannot install this ownership move; retain the source build and use the settled carry procedure",
        ));
    }
    Ok(())
}

fn write_capsule(path: &std::path::Path, bytes: &[u8]) -> CliResult<()> {
    use std::io::Write as _;
    let directory = path
        .parent()
        .ok_or_else(|| CliFailure::new("capsule has no directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(super::io_failure)?;
    file.write_all(bytes).map_err(super::io_failure)?;
    file.as_file().sync_all().map_err(super::io_failure)?;
    file.persist(path)
        .map_err(|error| super::io_failure(error.error))?;
    std::fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(super::io_failure)
}
