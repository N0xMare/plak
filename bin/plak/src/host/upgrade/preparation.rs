//! Preparation failures withdraw receipts before releasing source authority.

use super::{
    CliFailure, CliResult, PlakPaths, Ready, Services, check_cancel, io_failure, source_paths,
};
use crate::carry::upgrade::{
    crash_cut, process,
    receipt::{Mapping, Phase, Receipt},
};

pub(super) fn exports(
    services: &Services,
    ready: &Ready,
    receipt: &mut Receipt,
    documents: Vec<crate::store::sessions::SessionRecord>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> CliResult<()> {
    let paths = services.paths()?;
    let directory = receipt
        .path(paths)
        .parent()
        .ok_or_else(|| CliFailure::new("upgrade has no directory"))?
        .to_path_buf();
    for record in documents {
        check_cancel(cancelled)?;
        ready.leases.hold(services, &record)?;
        let record = services
            .session_records()?
            .load(&record.id)
            .map_err(CliFailure::new)?;
        let document = crate::carry::export_session(&source_paths(paths, &record), &record)
            .map_err(|error| {
                CliFailure::new(format!("host_upgrade_blocked: {}: {error}", record.id))
            })?;
        let file = directory.join(format!("{}.json", record.id));
        crate::carry::write_document(&file, &document).map_err(CliFailure::new)?;
        crash_cut("after-export");
        let mut capacity =
            process::command(&receipt.destination, paths.home(), &receipt.destination_key);
        capacity
            .args(["version", "--host-upgrade-capacity"])
            .arg(&file);
        process::run(capacity, super::PROBE_BOUND)?;
        let bytes = std::fs::read(&file).map_err(io_failure)?;
        receipt.mappings.push(Mapping {
            ready: false,
            source: record.id,
            source_node: plakous_host_protocol::wire::NodeAddress {
                tree_id: record.node.tree_id,
                agent_id: record.node.agent_id,
            },
            successor: uuid::Uuid::new_v4().to_string(),
            digest: plakous::model::sha256_hex(&[&bytes]),
        });
        receipt.save(paths)?;
        crash_cut("after-mapping");
        crate::test_hooks::upgrade_preparation_checkpoint("after-mapping")?;
    }
    process::verify_image(&receipt.destination, &receipt.destination_binary)?;
    check_cancel(cancelled)?;
    ready.leases.check()?;
    receipt.source_archival_pending = true;
    receipt.save(paths)?;
    archive_sources(paths, receipt, true)?;
    crash_cut("after-source-archive");
    check_cancel(cancelled)?;
    ready.leases.check()?;
    Ok(())
}

fn archive_sources(paths: &PlakPaths, receipt: &Receipt, archived: bool) -> CliResult<()> {
    if receipt.source_key != crate::home::state::state_key() {
        return Err(CliFailure::new(
            "source archival requires its own state key",
        ));
    }
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    for mapping in &receipt.mappings {
        product
            .set_archived(&mapping.source, archived)
            .map_err(|error| CliFailure::new(error.to_string()))?;
    }
    Ok(())
}

pub(super) fn withdraw(paths: &PlakPaths, receipt: &mut Receipt) -> CliResult<()> {
    let restored = archive_sources(paths, receipt, false);
    if restored.is_ok() {
        receipt.source_archival_pending = false;
    }
    receipt.phase = Phase::Aborted;
    let saved = receipt.save(paths);
    restored.and(saved)
}
