//! Revision work runs on private copies before a restart and on stopped
//! stores at startup, never on a live store.

mod cleanup;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::{Duration, Instant};

use std::os::unix::fs::DirBuilderExt as _;

use plakous::sessions::rehearsal::{
    bootstrap_stopped_runtime, rehearse_runtime_bootstrap, runtime_bootstrap_pending,
};
use plakous_recursive_subagents_store::admission::{LockedUpgrade, copy_snapshot_bounded};

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
    let _admission = crate::writers::admission(paths, true)?;
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

#[derive(serde::Serialize)]
pub(super) struct Rehearsal {
    /// Whether the product or tree store needs a revision this build applies.
    revisions_pending: bool,
    /// Whether the runtime store needs a stopped bootstrap or admission revision.
    runtime_bootstrap_pending: bool,
    /// The directory to pass to `--store-probe`.
    stores: std::path::PathBuf,
    elapsed_ms: u128,
}

const REHEARSAL_TIME: Duration = Duration::from_secs(120);
const REHEARSAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Copies the product and tree stores into `into/stores` and applies this
/// build's pending revisions there; rehearses a pending runtime bootstrap on
/// a copy too. The live stores are only read. The directory must not exist
/// and must sit directly under the product home, where `--store-probe` reads.
pub(super) fn rehearse_into(paths: &PlakPaths, into: &Path) -> CliResult<Rehearsal> {
    rehearse_with_budget(paths, into, REHEARSAL_TIME, REHEARSAL_BYTES)
}

fn rehearse_with_budget(
    paths: &PlakPaths,
    into: &Path,
    time: Duration,
    bytes: u64,
) -> CliResult<Rehearsal> {
    let start = Instant::now();
    let home = paths
        .home()
        .canonicalize()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let parent = into
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .ok_or_else(|| CliFailure::new("store rehearse needs an absolute --into path"))?;
    if parent != home {
        return Err(CliFailure::new(
            "store rehearse --into must name a new directory directly under the product home",
        ));
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(into)
        .map_err(|error| CliFailure::new(format!("cannot create {}: {error}", into.display())))?;
    let result = rehearse_copies(paths, into, start, start + time, bytes);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(into);
    }
    result
}

fn rehearse_copies(
    paths: &PlakPaths,
    into: &Path,
    start: Instant,
    deadline: Instant,
    mut remaining: u64,
) -> CliResult<Rehearsal> {
    if Instant::now() >= deadline {
        return Err(CliFailure::new("store rehearse: exceeded its time bound"));
    }
    let stores = into.join("stores");
    crate::commands::runner::private_directory(&stores)?;
    let revisions_pending = revisions_pending(paths)?;
    let copies = paths.clone().with_probe_stores(stores.clone());
    for ((schema, source), (_, destination)) in databases(paths).into_iter().zip(databases(&copies))
    {
        let used = copy_snapshot_bounded(&source, &destination, remaining, deadline)
            .map_err(snapshot_failure)?;
        remaining -= used;
        if used != 0 {
            schema
                .rehearse_copy(&destination, deadline)
                .map_err(failure)?;
        }
    }
    let runtime = paths.runtime_database();
    let runtime_bootstrap_pending = runtime_bootstrap_pending(&runtime)?;
    if runtime_bootstrap_pending {
        let scratch = into.join("runtime");
        crate::commands::runner::private_directory(&scratch)?;
        let copy = scratch.join("runtime.sqlite3");
        copy_snapshot_bounded(&runtime, &copy, remaining, deadline).map_err(snapshot_failure)?;
        rehearse_runtime_bootstrap(&copy, deadline)?;
    }
    if Instant::now() >= deadline {
        return Err(CliFailure::new("store rehearse: exceeded its time bound"));
    }
    Ok(Rehearsal {
        revisions_pending,
        runtime_bootstrap_pending,
        stores,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

fn snapshot_failure(
    error: plakous_recursive_subagents_store::admission::AdmissionError,
) -> CliFailure {
    CliFailure::new(format!("store rehearse: {error}"))
}
