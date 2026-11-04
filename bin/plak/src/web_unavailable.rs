//! `plak web` in a build without the `web` feature: a typed refusal.

use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

use crate::command::WebCommand;
use crate::shell::services::Services;

/// Refuses: this build has no web endpoint.
///
/// # Errors
///
/// Always.
pub(crate) fn run_web_command(
    _services: &Services,
    command: WebCommand,
) -> CliResult<ProcessOutcome> {
    // Parsed for every build, so a refusal here names the build, not a typo.
    let _ = (
        command.listen,
        command.ui,
        command.session,
        command.cwd,
        command.dev_login,
    );
    Err(CliFailure::new(
        "plak web is not in this build; maintainer builds have it (feature web)",
    ))
}
