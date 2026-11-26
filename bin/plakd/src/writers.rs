//! Standalone writer admission for a home (spec 02 §4.6, restart upgrades).
//!
//! Embedded and headless writers hold this lock shared. A stopped-store
//! revision step, a standalone repair or an operator's backup window takes it
//! exclusively, so no other writer can open the stores. Host client faces do
//! not take it.

use crate::{CliFailure, CliResult};

/// The lock keeps its historical file name so that a still-running older
/// standalone writer and this build fence each other.
const LOCK: &str = "run/upgrade-admission.lock";

pub(crate) fn admission(
    paths: &crate::home::paths::PlakPaths,
    exclusive: bool,
) -> CliResult<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt as _;
    crate::commands::runner::private_directory(&paths.home().join("run"))?;
    let descriptor = rustix::fs::open(
        paths.home().join(LOCK),
        OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| CliFailure::new(error.to_string()))?;
    let file = std::fs::File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(CliFailure::new(
            "writer admission lock is not a private owned regular file",
        ));
    }
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map(|()| file).map_err(|_| {
        CliFailure::new(
            "home_writers_busy: an embedded writer or an exclusive store operation owns this home; settle it first",
        )
    })
}
