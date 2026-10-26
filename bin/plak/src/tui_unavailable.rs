use crate::{CliResult, ProcessOutcome, shell::services::Services};

pub(crate) fn launch_tui(
    _services: &Services,
    _session_id: Option<String>,
    _resume_picker: bool,
    _note: Option<String>,
) -> CliResult<ProcessOutcome> {
    Err(plakous_hosted::capabilities::CapabilityUnavailable {
        id: "tui",
        kind: plakous_hosted::capabilities::CapabilityKind::Surface,
        feature: "tui",
    }
    .into())
}
