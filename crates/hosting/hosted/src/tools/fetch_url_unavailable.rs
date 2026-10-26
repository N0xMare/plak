use plakous::tools::ToolRegistration;

use crate::capabilities::{CapabilityKind, CapabilityUnavailable};
use crate::error::CliResult;

pub(super) fn registrations(
    _limits: plakous::tools::web::FetchUrlLimits,
) -> CliResult<Vec<ToolRegistration>> {
    Err(CapabilityUnavailable {
        id: "fetch_url",
        kind: CapabilityKind::ToolPack,
        feature: "web-fetch",
    }
    .into())
}
