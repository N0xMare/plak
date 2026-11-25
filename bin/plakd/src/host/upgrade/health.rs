use std::path::{Path, PathBuf};
use std::time::Instant;

pub(in crate::host) use crate::carry::upgrade::timing::ADMISSION as ADMISSION_TIME;
use crate::carry::upgrade::timing::STORE as STORE_TIME;
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

/// The checked stores, in the fixed order their failures are reported.
pub(in crate::host) fn databases(paths: &PlakPaths) -> [(&'static str, PathBuf); 3] {
    [
        ("product", paths.product_database()),
        ("tree", paths.subagents_database()),
        ("runtime", paths.runtime_database()),
    ]
}

pub(in crate::host) fn deadline() -> Instant {
    Instant::now() + STORE_TIME
}

pub(in crate::host) fn store(label: &str, path: &Path, deadline: Instant) -> CliResult<()> {
    if crate::product_doctor::upgrade_database_healthy(path, deadline) {
        return Ok(());
    }
    Err(CliFailure::new(format!(
        "host_upgrade_health_refused: {label} store did not pass the bounded integrity check"
    )))
}

pub(in crate::host) fn stores(paths: &PlakPaths) -> CliResult<()> {
    let deadline = deadline();
    for (label, path) in databases(paths) {
        store(label, &path, deadline)?;
    }
    Ok(())
}
