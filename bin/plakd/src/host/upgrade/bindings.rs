//! Upgrade binding regressions, not pre-existing or external availability failures.

use crate::carry::ChildReopenRefusal;
use crate::commands::session::check_binding::{Report, Row};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

// The explicit 200-head / 28.5 MB retained-state measurement emits 49 KB in
// about 14 seconds. Leave headroom without making a pre-surrender probe unbounded.
const OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
pub(in crate::host) const INSPECTION_TIME: Duration = Duration::from_secs(120);

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

#[derive(Deserialize, Serialize)]
pub(in crate::host) struct Baseline(Option<BTreeSet<String>>);

fn comparable(row: &Row) -> bool {
    !row.archived && !row.superseded && !row.closed
}

impl Baseline {
    fn from_report(report: Option<&Report>) -> Self {
        Self(report.map(|report| {
            report
                .sessions
                .iter()
                .filter(|row| {
                    comparable(row) && row.binding_change.is_none() && row.unavailable.is_none()
                })
                .map(binding_key)
                .collect()
        }))
    }
}

fn binding_key(row: &Row) -> String {
    use sha2::{Digest as _, Sha256};
    let mut digest = Sha256::new();
    for field in [&row.session, &row.branch] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    crate::shipping_artifact::hex_digest(&digest.finalize())
}

pub(super) fn requester_baseline(
    paths: &PlakPaths,
    identity: &super::Identity,
) -> CliResult<Baseline> {
    let instance = plakous_recursive_subagents::tree::HostInstance::parse(&identity.instance)
        .map_err(CliFailure::new)?;
    let image = source_image(instance.pid())?;
    let verified = super::process::verify_image(&image, &identity.binary)?;
    let output = probe(&image, paths, &identity.key, None)?;
    verified.recheck(&image)?;
    let report = serde_json::from_slice::<Report>(&output.stdout).ok();
    Ok(Baseline::from_report(report.as_ref()))
}

#[cfg(target_os = "linux")]
fn source_image(pid: u32) -> CliResult<PathBuf> {
    // The descriptor path resolves the running bytes even if the installed
    // link has been switched ahead of the process.
    let path = PathBuf::from(format!("/proc/{pid}/exe"));
    path.metadata().map_err(super::io_failure)?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn source_image(pid: u32) -> CliResult<PathBuf> {
    let mut command = std::process::Command::new("/bin/ps");
    command.args(["-ww", "-p", &pid.to_string(), "-o", "comm="]);
    let output = super::process::run(command, super::PROBE_BOUND)?;
    let image = String::from_utf8(output.stdout)
        .map_err(|_| CliFailure::new("invalid source image path"))?;
    let path = PathBuf::from(image.trim_end_matches('\n'));
    if !path.is_absolute() {
        return Err(CliFailure::new(
            "host upgrade source image path is unavailable",
        ));
    }
    Ok(path)
}

pub(in crate::host) fn health(paths: &PlakPaths, baseline: &Baseline) -> CliResult<()> {
    let image = std::env::current_exe().map_err(super::io_failure)?;
    let output = probe(&image, paths, &crate::home::state::state_key(), None)?;
    let report = serde_json::from_slice::<Report>(&output.stdout).map_err(|_| {
        CliFailure::new(
            "host_upgrade_health_refused: destination binding inspection gave no verdict",
        )
    })?;
    // A working head can be unavailable until planned recovery. The paired
    // exact-reopen and preservation checks own that case, not a strict fleet
    // binding audit or a second attempt to open the live session.
    let refused = regressions(baseline, &report);
    if refused.is_empty() {
        Ok(())
    } else {
        Err(CliFailure::new(format!(
            "host_upgrade_health_refused: destination binding compatibility did not pass{}",
            child_refusal(&refused),
        )))
    }
}

fn regressions<'a>(baseline: &Baseline, destination: &'a Report) -> Vec<&'a Row> {
    destination
        .sessions
        .iter()
        .filter(|row| {
            if !comparable(row) || row.binding_change.is_none() && row.unavailable.is_none() {
                return false;
            }
            match &baseline.0 {
                None => row.binding_change.is_some(),
                Some(source) => source.contains(&binding_key(row)),
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
) -> CliResult<Baseline> {
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

fn finish(source: Option<&Report>, destination: &Report) -> CliResult<Baseline> {
    let baseline = Baseline::from_report(source);
    let refused = regressions(&baseline, destination);
    if refused.is_empty() {
        return Ok(baseline);
    }
    refusal(source.is_some(), &refused)
}

fn child_refusal(refused: &[&Row]) -> String {
    let mut children = ChildReopenRefusal::default();
    for row in refused.iter().filter(|row| row.child) {
        children.record(row.binding_change.as_deref(), row.unavailable.as_deref());
    }
    children
        .into_report()
        .map_or_else(String::new, |refusal| format!("; {refusal}"))
}

fn refusal<T>(baseline_available: bool, refused: &[&Row]) -> CliResult<T> {
    let children = child_refusal(refused);
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
