//! Upgrade binding regressions, not pre-existing or external availability failures.

use crate::commands::session::check_binding::{Report, Row};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};
use std::{collections::BTreeSet, path::Path, time::Duration};

// The explicit 200-head / 28.5 MB retained-state measurement emits 49 KB in
// about 14 seconds. Leave headroom without making a pre-surrender probe unbounded.
const OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const INSPECTION_TIME: Duration = Duration::from_secs(120);

#[cfg(test)]
mod tests;

fn probe(image: &Path, paths: &PlakPaths, key: &str) -> CliResult<std::process::Output> {
    let mut command = super::process::command(image, paths.home(), key);
    command.args(["session", "check-binding", "--all"]);
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

pub(super) fn preflight(paths: &PlakPaths, destination: &Path, key: &str) -> CliResult<()> {
    let source = std::env::current_exe().map_err(|error| CliFailure::new(error.to_string()))?;
    let baseline = probe(&source, paths, key)?;
    let baseline = serde_json::from_slice::<Report>(&baseline.stdout).ok();
    if baseline.is_none() {
        eprintln!(
            "plak host: binding baseline unavailable; source did not answer check-binding; no source verdict inferred"
        );
    }
    let output = probe(destination, paths, key)?;
    let destination: Report = serde_json::from_slice(&output.stdout).map_err(|_| {
        CliFailure::new(format!(
            "host_upgrade_binding_preflight_invalid: {}\n{}",
            crate::cli_output::safe_terminal_block(&String::from_utf8_lossy(&output.stderr)),
            crate::cli_output::safe_terminal_block(&String::from_utf8_lossy(&output.stdout))
        ))
    })?;
    finish(baseline.as_ref(), &destination)
}

/// Busy-safe source-store inspection, not a capsule or runtime reopen.
pub(super) fn preflight_selected(
    paths: &PlakPaths,
    destination: &Path,
    sessions: &BTreeSet<String>,
) -> CliResult<()> {
    let inspection = crate::commands::session::check_binding::inspection::capture(paths, sessions)?;
    crate::commands::runner::private_directory(&paths.home().join("run"))?;
    let mut file = tempfile::NamedTempFile::new_in(paths.home().join("run"))
        .map_err(|error| CliFailure::new(error.to_string()))?;
    std::io::Write::write_all(&mut file, &inspection.bytes()?)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let baseline = inspection.check()?;
    let mut command =
        super::process::command(destination, paths.home(), &crate::home::state::state_key());
    command
        .args(["session", "check-binding", "--all", "--inspection"])
        .arg(file.path());
    let output = super::process::run_output(command, INSPECTION_TIME, OUTPUT_BYTES)?;
    let destination = serde_json::from_slice(&output.stdout).map_err(|_| {
        CliFailure::new("host_upgrade_binding_preflight_invalid: source remains running; selected-head inspection did not answer")
    })?;
    qualify_selected(baseline, destination, sessions)
}

fn qualify_selected(
    mut source: Report,
    mut destination: Report,
    sessions: &BTreeSet<String>,
) -> CliResult<()> {
    source
        .sessions
        .retain(|row| sessions.contains(&row.session));
    destination
        .sessions
        .retain(|row| sessions.contains(&row.session));
    if sessions
        .iter()
        .any(|id| !source.sessions.iter().any(|row| &row.session == id))
        || source
            .sessions
            .iter()
            .filter(|row| !row.archived && !row.superseded)
            .any(|old| {
                !destination.sessions.iter().any(|row| {
                    row.session == old.session
                        && row.branch == old.branch
                        && !row.archived
                        && !row.superseded
                })
            })
    {
        return Err(CliFailure::new(
            "host_upgrade_binding_preflight_incomplete: selected active heads were omitted; source remains running",
        ));
    }
    let cuts = destination
        .sessions
        .iter()
        .filter(|row| !row.archived && !row.superseded && row.binding_change.is_some())
        .collect::<Vec<_>>();
    if !cuts.is_empty() {
        return refusal(true, &cuts);
    }
    finish(Some(&source), &destination)
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
