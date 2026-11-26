//! `plakd host` in a build without the `web` feature: a typed refusal.

use plakous_hosted::error::{CliResult, ProcessOutcome};

use crate::shell::services::Services;

pub(crate) const AVAILABLE: bool = false;

pub(crate) fn abort_child_retirement(
    _paths: &crate::product_paths::PlakPaths,
    _tree: &str,
    _child: &str,
    _stdout: &mut dyn std::io::Write,
) -> CliResult<()> {
    Err(plakous_hosted::error::CliFailure::new(
        "stopped-home retirement abort is unavailable",
    ))
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

pub(crate) fn wake(services: &Services, _session: &str, _text: &str) -> CliResult<ProcessOutcome> {
    run_host_command(services, &crate::command::HostCommand::default())
}

pub(crate) fn rehearse_stores(
    services: &Services,
    _into: &std::path::Path,
    _stdout: &mut dyn std::io::Write,
) -> CliResult<ProcessOutcome> {
    run_host_command(services, &crate::command::HostCommand::default())
}
