use plakous::tools::{ToolPack, ToolRegistration};

use crate::error::{CliFailure, CliResult};

pub(super) fn registrations(
    limits: plakous::tools::web::FetchUrlLimits,
    image_results: bool,
) -> CliResult<Vec<ToolRegistration>> {
    let pack =
        plakous::tools::web::FetchUrlPack::new(limits, &format!("plak/{}", plakous::VERSION))
            .map_err(|error| CliFailure::new(format!("fetch_url: {error}")))?
            .with_image_results(image_results);
    pack.registrations()
        .map_err(|error| CliFailure::new(format!("fetch_url: {error}")))
}
