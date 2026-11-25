//! Revision work runs on scratch during qualification and on stopped stores
//! after handoff, never on a live source.

mod cleanup;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::path::Path;

use plakous::sessions::rehearsal::{
    bootstrap_stopped_runtime, rehearse_runtime_bootstrap, runtime_bootstrap_pending,
};
use plakous_recursive_subagents_store::admission::LockedUpgrade;

use crate::home::paths::PlakPaths;
use crate::store::revisions::{
    databases,
    host::{failure, pending as revisions_pending},
};
use crate::{CliFailure, CliResult};

pub(super) fn pending(paths: &PlakPaths) -> CliResult<bool> {
    let runtime = runtime_bootstrap_pending(&paths.runtime_database())?;
    Ok(revisions_pending(paths)? || runtime)
}

pub(super) fn runtime_pending(paths: &PlakPaths) -> CliResult<bool> {
    Ok(runtime_bootstrap_pending(&paths.runtime_database())?)
}

pub(super) fn upgrade_stopped(
    paths: &PlakPaths,
    needs_upgrade: bool,
    _state_lock: &super::lock::HostLock,
) -> CliResult<()> {
    cleanup::stale_copies(paths);
    if !needs_upgrade {
        return Ok(());
    }
    // Exclude pre-existing embedded writers before taking either SQLite lock.
    let _admission = crate::carry::upgrade::admission(paths, true)?;
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
    bootstrap_stopped_runtime(&paths.runtime_database(), |point| {
        crate::test_hooks::upgrade_crash_cut(&format!("runtime:{point}"));
    })?;
    for upgrade in backups {
        upgrade
            .commit(|stem, point| crate::test_hooks::upgrade_crash_cut(&format!("{stem}:{point}")))
            .map_err(failure)?;
    }
    Ok(())
}

const REQUEST: &str = "scratch-request";

pub(super) fn request_rehearsal(directory: &Path) -> CliResult<File> {
    let marker = File::create_new(directory.join(REQUEST))
        .map_err(|error| CliFailure::new(error.to_string()))?;
    marker
        .lock_shared()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    Ok(marker)
}

pub(super) fn rehearse_for_service(paths: &PlakPaths, service: &Path) -> CliResult<()> {
    let marker = service.with_file_name(REQUEST);
    if marker.is_file() {
        let _probe = cleanup::lock_marker(&marker, false)
            .ok_or_else(|| CliFailure::new("store qualification scratch request is unavailable"))?;
        return rehearse(paths, &service.with_file_name("stores"));
    }
    // Older coordinators can't route their later probes to the copies.
    // Equal-revision cutovers stay compatible in either direction.
    let runtime = runtime_bootstrap_pending(&paths.runtime_database())?;
    if (runtime
        || databases(paths)
            .iter()
            .any(|(schema, _)| schema.version() > schema.generation))
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

/// The destination requester runs this before sending a request, even when
/// the live coordinator predates the runtime shape change.
pub(super) fn rehearse_runtime(paths: &PlakPaths) -> CliResult<()> {
    let runtime = paths.runtime_database();
    if !runtime_bootstrap_pending(&runtime)? {
        return Ok(());
    }
    eprintln!("host upgrade: qualifying runtime store bootstrap on a detached copy");
    let directory =
        tempfile::tempdir_in(paths.home()).map_err(|error| CliFailure::new(error.to_string()))?;
    let _marker = request_rehearsal(directory.path())?;
    let stores = directory.path().join("stores");
    crate::commands::runner::private_directory(&stores)?;
    let start = std::time::Instant::now();
    rehearse_runtime_bootstrap(&runtime, &stores.join("runtime.sqlite3"))?;
    eprintln!(
        "host upgrade: runtime store bootstrap qualification completed in {} ms",
        start.elapsed().as_millis()
    );
    Ok(())
}

pub(super) fn rehearse(paths: &PlakPaths, scratch: &Path) -> CliResult<()> {
    if !revisions_pending(paths)? {
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
