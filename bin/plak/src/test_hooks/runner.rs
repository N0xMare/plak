//! Controlled runner transport and awake-time inputs for real-process proofs.

use std::path::Path;
use std::sync::Arc;

mod link;
pub(crate) use link::runner_link;

#[allow(
    dead_code,
    reason = "only builds with the host start detached fixtures"
)]
pub(crate) fn host_start_environment() -> Vec<(&'static str, std::ffi::OsString)> {
    [
        "PLAK_TEST_RUNNER_DETACHED",
        "PLAK_TEST_MODEL",
        "PLAK_TEST_SCRIPT",
        "PLAK_TEST_EVIDENCE",
        "PLAK_TEST_HOST_IDLE_EXIT_SECONDS",
    ]
    .into_iter()
    .filter_map(|name| super::hook_environment(name).map(|value| (name, value)))
    .collect()
}

#[allow(dead_code, reason = "only the optional host dials its local runner")]
pub(crate) fn runner_windows() -> plakous::runner::ClientWindows {
    let window = super::selected()
        .and_then(Result::ok)
        .and_then(|selection| selection.plan.runner_windows.clone());
    window.map_or_else(plakous::runner::ClientWindows::default, |windows| {
        plakous::runner::ClientWindows {
            read: std::time::Duration::from_millis(windows.read_ms),
            effect: std::time::Duration::from_millis(windows.effect_ms),
            ping: std::time::Duration::from_millis(windows.ping_ms),
            dead: std::time::Duration::from_millis(windows.dead_ms),
        }
    })
}

pub(crate) async fn runner_before_ready(
    client: &plakous::runner::RunnerClient,
) -> Result<(), plakous::runner::PlacementFailure> {
    let Some(directory) = hold_cuts() else {
        return Ok(());
    };
    if directory.join("before-ready.armed").is_file()
        && link::claim(&directory.join("before-ready.entered"))
    {
        link::release(&directory.join("before-ready.release"), None, Some(client))
            .await
            .map_err(|_| plakous::runner::PlacementFailure::RunnerUnreachable {
                phase: plakous::runner::ProbePhase::Reconcile,
                timed_out: true,
            })?;
    }
    Ok(())
}

pub(crate) fn runner_ready_result(result: &Result<(), plakous::runner::PlacementFailure>) {
    if let Some(directory) = hold_cuts()
        && directory.join("before-ready.entered").is_file()
    {
        let verdict = match result {
            Ok(()) => "ready",
            Err(plakous::runner::PlacementFailure::RunnerUnreachable {
                phase: plakous::runner::ProbePhase::LinkLost,
                ..
            }) => "link_lost",
            Err(_) => "blocked",
        };
        let _ = std::fs::write(directory.join("before-ready.result"), verdict);
    }
}

fn hold_cuts() -> Option<std::path::PathBuf> {
    super::selected()
        .and_then(Result::ok)
        .and_then(|selection| selection.plan.runner_hold_cuts.clone())
}

/// Owner-only fixture ticks let strict sweeps distinguish suspend from awake
/// lease lapse without changing the machine clock or waiting five minutes.
pub(crate) fn runner_lease_clock(home: &Path) -> Option<Arc<plakous::runner::RunnerLeaseClock>> {
    let path = home.join("lease-clock-ms");
    if !path.is_file() {
        return None;
    }
    let origin = std::time::Instant::now();
    Some(Arc::new(move || {
        let elapsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        origin + std::time::Duration::from_millis(elapsed)
    }))
}
