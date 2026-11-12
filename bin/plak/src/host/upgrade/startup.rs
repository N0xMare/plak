//! Only the destination holding home custody imports surrendered roots before Ready.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crate::carry::upgrade::{
    crash_cut,
    receipt::{self, Phase, Receipt},
};
use crate::commands::runner::services::host as runner;
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};

pub(in crate::host) fn prepare_host(
    services: &Services,
) -> CliResult<(Services, Arc<plakous::runner::WorkspaceCatalog>)> {
    let services = runner::custody(services)?;
    for receipt in pending(&services)? {
        crash_cut("before-import");
        crate::carry::upgrade::import(&services, &receipt.path(services.paths()?))?;
        crash_cut("after-import");
    }
    runner::prepare(&services, None)
}

fn pending(services: &Services) -> CliResult<Vec<Receipt>> {
    Ok(receipt::all(services.paths()?)?
        .into_iter()
        .filter(|receipt| {
            receipt.destination_key == crate::home::state::state_key()
                && receipt.destination_binary == crate::build_identity::binary_id()
                && receipt.phase == Phase::Committed
        })
        .collect())
}

pub(in crate::host) fn acknowledge(
    services: &Services,
    runtime: &tokio::runtime::Runtime,
) -> CliResult<()> {
    let pending = pending(services)?;
    if pending.is_empty() {
        return Ok(());
    }
    let mut required = BTreeSet::new();
    for receipt in &pending {
        for mapping in &receipt.mappings {
            let record = services
                .session_records()?
                .load(&mapping.successor)
                .map_err(CliFailure::new)?;
            if record.plakous_locator.is_none() {
                return Err(CliFailure::new("upgrade successor import is incomplete"));
            }
            required.insert(
                record
                    .runner
                    .map_or_else(|| "local".to_owned(), |runner| runner.runner_id),
            );
        }
    }
    for id in &required {
        services.runners().require_connection(id, true);
    }
    let ids = required.into_iter().collect::<Vec<_>>();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(60), services.runners().refresh(Some(&ids))).await
            .map_err(|_| CliFailure::new("host_upgrade_destination_not_ready: recover through the recorded destination receipt"))?
            .map_err(|failure| CliFailure::new(format!("host_upgrade_destination_not_ready: {failure:?}; recover through the destination receipt")))
    })?;
    if ids.iter().any(|id| {
        !services.runners().rows().iter().any(|row| {
            row.runner.runner_id == *id && row.status == plakous::runner::RunnerStatus::Ready
        })
    }) {
        return Err(CliFailure::new(
            "host upgrade runner readiness was not published; retry the recorded destination",
        ));
    }
    crash_cut("before-ready");
    for mut receipt in pending {
        receipt.phase = Phase::Acknowledged;
        receipt.save(services.paths()?)?;
        crash_cut("after-ack");
    }
    Ok(())
}
