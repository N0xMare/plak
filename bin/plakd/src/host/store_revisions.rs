//! Revision work runs on scratch during qualification and on stopped stores
//! after handoff, never on a live source.

use std::path::Path;

use plakous_recursive_subagents_store::admission::LockedUpgrade;

use crate::home::paths::PlakPaths;
use crate::store::revisions::host::{databases, failure, pending};
use crate::{CliFailure, CliResult};

pub(super) fn upgrade_stopped(
    paths: &PlakPaths,
    needs_upgrade: bool,
    _state_lock: &super::lock::HostLock,
) -> CliResult<()> {
    if !needs_upgrade {
        return Ok(());
    }
    // Exclude pre-existing embedded writers before taking either SQLite lock.
    let _admission = crate::carry::upgrade::admission(paths, true)?;
    pending(paths)?;
    let databases = databases(paths);
    let locks = databases
        .iter()
        .filter_map(|(schema, path)| schema.lock_upgrade(path).transpose())
        .collect::<Result<Vec<_>, _>>()
        .map_err(failure)?;
    let backups = locks
        .into_iter()
        .map(LockedUpgrade::backup)
        .collect::<Result<Vec<_>, _>>()
        .map_err(failure)?;
    for upgrade in backups {
        upgrade
            .commit(|stem, point| crate::test_hooks::upgrade_crash_cut(&format!("{stem}:{point}")))
            .map_err(failure)?;
    }
    Ok(())
}

const REQUEST: &str = "scratch-request";

pub(super) fn request_rehearsal(directory: &Path) -> CliResult<()> {
    std::fs::File::create_new(directory.join(REQUEST))
        .map(drop)
        .map_err(|error| CliFailure::new(error.to_string()))
}

pub(super) fn rehearse_for_service(paths: &PlakPaths, service: &Path) -> CliResult<()> {
    if service.with_file_name(REQUEST).is_file() {
        return rehearse(paths, &service.with_file_name("stores"));
    }
    // Older coordinators can't route their later probes to the copies.
    // Equal-revision cutovers stay compatible in either direction.
    if databases(paths)
        .iter()
        .any(|(schema, _)| schema.version() > schema.generation)
        && crate::wire_transport::hosts::live(paths)?
            .iter()
            .any(|host| host.key == crate::home::state::state_key())
        && pending(paths)?
    {
        return Err(CliFailure::new(
            "host_upgrade_destination_refused: the source cannot qualify store revisions; stop it and start the destination build",
        ));
    }
    Ok(())
}

fn rehearse(paths: &PlakPaths, scratch: &Path) -> CliResult<()> {
    if !pending(paths)? {
        return Ok(());
    }
    crate::commands::runner::private_directory(scratch)?;
    let copies = paths.clone().with_probe_stores(scratch.to_path_buf());
    for ((schema, source), (_, destination)) in databases(paths).into_iter().zip(databases(&copies))
    {
        schema.rehearse(&source, &destination).map_err(failure)?;
    }
    Ok(())
}
