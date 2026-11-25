//! Upgrade binding regressions, not pre-existing or external availability failures.

use crate::carry::ChildReopenRefusal;
use crate::commands::session::check_binding::{Report, Row};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};
use std::{collections::HashSet, path::Path, time::Duration};

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
    let comparable = |row: &Row| !row.archived && !row.superseded && !row.closed;
    let source = source.map(|report| {
        report
            .sessions
            .iter()
            .filter(|row| {
                comparable(row) && row.binding_change.is_none() && row.unavailable.is_none()
            })
            .map(|row| (row.session.as_str(), row.branch.as_str()))
            .collect::<HashSet<_>>()
    });
    destination
        .sessions
        .iter()
        .filter(|row| {
            if !comparable(row) || row.binding_change.is_none() && row.unavailable.is_none() {
                return false;
            }
            match &source {
                None => row.binding_change.is_some(),
                Some(source) => source.contains(&(row.session.as_str(), row.branch.as_str())),
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
    let (baseline, output) = std::thread::scope(|scope| {
        let baseline = std::thread::Builder::new()
            .spawn_scoped(scope, || {
                super::progress::timed(timing, super::progress::Phase::BindingSource, || {
                    probe(&source, paths, key, None)
                })
            })
            .map_err(|error| {
                CliFailure::new(format!("could not start binding baseline: {error}"))
            })?;
        let output =
            super::progress::timed(timing, super::progress::Phase::BindingDestination, || {
                probe(destination, paths, key, scratch)
            });
        let baseline = baseline
            .join()
            .map_err(|_| CliFailure::new("binding baseline worker panicked"))??;
        Ok::<_, CliFailure>((baseline, output))
    })?;
    let baseline = serde_json::from_slice::<Report>(&baseline.stdout).ok();
    if baseline.is_none() {
        eprintln!(
            "plakd host: binding baseline unavailable; source did not answer check-binding; no source verdict inferred"
        );
    }
    let output = output?;
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
    let mut children = ChildReopenRefusal::default();
    for row in refused.iter().filter(|row| row.child) {
        children.record(row.binding_change.as_deref(), row.unavailable.as_deref());
    }
    let children = children
        .into_report()
        .map_or_else(String::new, |refusal| format!("; {refusal}"));
    Err(CliFailure::new(format!(
        "host_upgrade_binding_preflight_refused: source remains running; baseline {}; offending rows: {}{children}",
        if baseline_available {
            "available"
        } else {
            "unavailable"
        },
        serde_json::to_string(&refused)
            .map_err(|_| CliFailure::new("binding refusal could not be printed"))?,
    )))
}
