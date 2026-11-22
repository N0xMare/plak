//! Upgrade binding regressions, not pre-existing or external availability failures.

use crate::commands::session::check_binding::{Report, Row};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};
use std::{path::Path, time::Duration};

// The explicit 200-head / 28.5 MB retained-state measurement emits 49 KB in
// about 14 seconds. Leave headroom without making a pre-surrender probe unbounded.
const OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const INSPECTION_TIME: Duration = Duration::from_secs(120);

#[cfg(test)]
mod tests;

fn probe(
    image: &Path,
    paths: &PlakPaths,
    key: &str,
    scratch: Option<&Path>,
) -> CliResult<std::process::Output> {
    let mut command = super::process::command(image, paths.home(), key);
    command.args(["session", "check-binding", "--all"]);
    if let Some(scratch) = scratch {
        command.arg("--store-probe").arg(scratch);
    }
    super::process::run_output(command, INSPECTION_TIME, OUTPUT_BYTES)
}

pub(super) fn regressions<'a>(source: Option<&Report>, destination: &'a Report) -> Vec<&'a Row> {
    destination
        .sessions
        .iter()
        .filter(|row| {
            if row.archived
                || row.superseded
                || row.binding_change.is_none() && row.unavailable.is_none()
            {
                return false;
            }
            match source {
                None => row.binding_change.is_some(),
                Some(source) => source.sessions.iter().any(|old| {
                    old.session == row.session
                        && old.branch == row.branch
                        && !old.archived
                        && !old.superseded
                        && old.binding_change.is_none()
                        && old.unavailable.is_none()
                }),
            }
        })
        .collect()
}

pub(super) fn preflight(
    paths: &PlakPaths,
    destination: &Path,
    key: &str,
    scratch: Option<&Path>,
    timing: &super::progress::Report<'_>,
) -> CliResult<()> {
    let source = std::env::current_exe().map_err(|error| CliFailure::new(error.to_string()))?;
    let baseline = super::progress::timed(timing, super::progress::Phase::BindingSource, || {
        probe(&source, paths, key, None)
    })?;
    let baseline = serde_json::from_slice::<Report>(&baseline.stdout).ok();
    if baseline.is_none() {
        eprintln!(
            "plakd host: binding baseline unavailable; source did not answer check-binding; no source verdict inferred"
        );
    }
    let output =
        super::progress::timed(timing, super::progress::Phase::BindingDestination, || {
            probe(destination, paths, key, scratch)
        })?;
    let destination: Report = serde_json::from_slice(&output.stdout).map_err(|_| {
        CliFailure::new(format!(
            "host_upgrade_binding_preflight_invalid: {}\n{}",
            crate::cli_output::safe_terminal_block(&String::from_utf8_lossy(&output.stderr)),
            crate::cli_output::safe_terminal_block(&String::from_utf8_lossy(&output.stdout))
        ))
    })?;
    finish(baseline.as_ref(), &destination)
}

fn finish(baseline: Option<&Report>, destination: &Report) -> CliResult<()> {
    let refused = regressions(baseline, destination);
    if refused.is_empty() {
        return Ok(());
    }
    refusal(baseline.is_some(), &refused)
}

fn refusal(baseline_available: bool, refused: &[&Row]) -> CliResult<()> {
    Err(CliFailure::new(format!(
        "host_upgrade_binding_preflight_refused: source remains running; baseline {}; offending rows: {}",
        if baseline_available {
            "available"
        } else {
            "unavailable"
        },
        serde_json::to_string(&refused)
            .map_err(|_| CliFailure::new("binding refusal could not be printed"))?,
    )))
}
