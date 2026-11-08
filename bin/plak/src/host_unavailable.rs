//! `plak host` in a build without the `web` feature: a typed refusal.

use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

use crate::shell::services::Services;

pub(crate) const AVAILABLE: bool = false;

/// Refuses: this build has no host.
///
/// # Errors
///
/// Always.
pub(crate) fn run_host_command(
    _services: &Services,
    _command: &crate::command::HostCommand,
) -> CliResult<ProcessOutcome> {
    Err(CliFailure::new(
        "plak host is not in this build; maintainer builds have it (feature web)",
    ))
}
