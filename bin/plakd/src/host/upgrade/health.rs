use std::time::Instant;

pub(in crate::host) use crate::carry::upgrade::timing::ADMISSION as ADMISSION_TIME;
use crate::carry::upgrade::timing::STORE as STORE_TIME;
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

pub(in crate::host) fn stores(paths: &PlakPaths) -> CliResult<()> {
    let deadline = Instant::now() + STORE_TIME;
    for (label, path) in [
        ("product", paths.product_database()),
        ("tree", paths.subagents_database()),
        ("runtime", paths.runtime_database()),
    ] {
        if !crate::product_doctor::upgrade_database_healthy(&path, deadline) {
            return Err(CliFailure::new(format!(
                "host_upgrade_health_refused: {label} store did not pass the bounded integrity check"
            )));
        }
    }
    Ok(())
}
