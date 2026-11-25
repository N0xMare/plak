use std::time::{Duration, Instant};

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

pub(in crate::host) fn stores(paths: &PlakPaths) -> CliResult<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
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
