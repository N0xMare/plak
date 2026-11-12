//! Explicit offline producers for clients observing an otherwise ordinary host.
use std::io::Write;

mod abandoned;
mod pending;
pub(super) use pending::install_pending_gate;

pub(crate) fn seed_request(
    arguments: &[std::ffi::OsString],
    launch: Option<&std::path::Path>,
) -> Option<crate::CliResult<crate::ProcessOutcome>> {
    if arguments.first()?.to_str()? != "--test-seed-abandoned-open" {
        return None;
    }
    Some((|| {
        let with_submission = match arguments.get(1).and_then(|value| value.to_str()) {
            Some("with-submission") if arguments.len() == 2 => true,
            Some("without-submission") if arguments.len() == 2 => false,
            _ => {
                return Err(crate::CliFailure::new(
                    "fixture expects with-submission or without-submission",
                ));
            }
        };
        let receipt = abandoned::seed(
            launch.ok_or("fixture requires a workspace")?,
            with_submission,
        )?;
        let mut output = serde_json::to_vec(&receipt).map_err(|error| error.to_string())?;
        output.push(b'\n');
        std::io::stdout()
            .write_all(&output)
            .map_err(|error| error.to_string())?;
        Ok(crate::ProcessOutcome::Success)
    })())
}
