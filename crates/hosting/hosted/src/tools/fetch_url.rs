use plakous::tools::{ToolPack, ToolRegistration};

use crate::error::{CliFailure, CliResult};

pub(super) fn registrations(
    limits: plakous::tools::web::FetchUrlLimits,
) -> CliResult<Vec<ToolRegistration>> {
    let pack =
        plakous::tools::web::FetchUrlPack::new(limits, &format!("plak/{}", plakous::VERSION))
            .map_err(|error| CliFailure::new(format!("fetch_url: {error}")))?;
    pack.registrations()
        .map_err(|error| CliFailure::new(format!("fetch_url: {error}")))
}
