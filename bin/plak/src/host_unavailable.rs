//! `plak host` in a build without the `web` feature: a typed refusal.

use plakous_hosted::error::{CliResult, ProcessOutcome};

use crate::shell::services::Services;

pub(crate) const AVAILABLE: bool = false;

pub(crate) fn preflight_upgrade_service(
    services: &Services,
    command: &crate::command::HostCommand,
) -> CliResult<()> {
    run_host_command(services, command).map(|_| ())
}

/// Refuses: this build has no host.
///
/// # Errors
///
/// Always.
pub(crate) fn run_host_command(
    _services: &Services,
    _command: &crate::command::HostCommand,
) -> CliResult<ProcessOutcome> {
    Err(plakous_hosted::capabilities::CapabilityUnavailable {
        id: "host",
        kind: plakous_hosted::capabilities::CapabilityKind::Surface,
        feature: "web",
    }
    .into())
}

pub(crate) fn connect_stdio(services: &Services, _auto_start: bool) -> CliResult<ProcessOutcome> {
    run_host_command(services, &crate::command::HostCommand::default())
}

pub(crate) fn upgrade(
    services: &Services,
    _expect_binary: Option<&str>,
    _wait: std::time::Duration,
    _abandon: Option<&str>,
) -> CliResult<ProcessOutcome> {
    run_host_command(services, &crate::command::HostCommand::default())
}
