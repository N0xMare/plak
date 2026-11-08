use crate::{CliResult, ProcessOutcome, shell::services::Services};

pub(crate) const HOST_RESUME_SUPPORTED: bool = false;

pub(crate) fn launch_placed_tui(
    services: &Services,
    _runner: Option<&str>,
    _workspace: Option<&str>,
) -> CliResult<ProcessOutcome> {
    launch_tui(services, None, false, None)
}

pub(crate) fn launch_host_tui(
    services: &Services,
    session_id: Option<String>,
) -> CliResult<ProcessOutcome> {
    launch_tui(services, session_id, false, None)
}

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
